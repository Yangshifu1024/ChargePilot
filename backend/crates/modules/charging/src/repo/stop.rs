//! 用户停止的表访问：charge_order / charge_start_receipt / charge_billing_cutoff /
//! charge_stop_command / charge_command（蓝图第五章 repo 层）。

use chargepilot_domain::charging::command::{ChargeCommandStatus, ChargeStopCommandStatus};
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 活跃停止授权行（旧版 FindActiveStopAuthorization 投影）。
pub type ActiveStopRow = (u64, String, u64, String, u8, String);

/// 授权查询：属于用户、charging、有成功启动回执。
pub async fn find_active_stop(
    pool: &MySqlPool,
    order_no: &str,
    user_id: u64,
) -> Result<Option<ActiveStopRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.`id`, c.`order_no`, c.`user_id`, c.`device_id`, c.`port_no`, CAST(s.`command_id` AS CHAR) \
         FROM `charge_order` c \
         JOIN `charge_start_receipt` s ON s.`charge_order_id` = c.`id` AND s.`success` = 1 \
         WHERE c.`order_no` = ? AND c.`user_id` = ? AND c.`status` = ? \
           AND c.`deleted_at` IS NULL LIMIT 1",
    )
    .bind(order_no)
    .bind(user_id as i64)
    .bind(ChargeOrderStatus::Charging)
    .fetch_optional(pool)
    .await
}

/// 计费截止冻结（no-op upsert：首次冻结，重试不延长）。
pub async fn upsert_cutoff_tx(c: &mut MySqlConnection, order_id: u64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_billing_cutoff` (`charge_order_id`, `cutoff_at`, `reason`) \
         VALUES (?, UTC_TIMESTAMP(3), 'stop_request') \
         ON DUPLICATE KEY UPDATE `charge_order_id` = `charge_order_id`",
    )
    .bind(order_id as i64)
    .execute(c)
    .await?;
    Ok(())
}

/// 已有停止命令则复用（幂等重放）；取最新一条（BB 应答消费同源）。
pub async fn find_stop_command_tx<'e, E>(c: E, order_id: u64) -> Result<Option<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`command_id` AS CHAR) FROM `charge_stop_command` \
         WHERE `charge_order_id` = ? ORDER BY `created_at` DESC LIMIT 1",
    )
    .bind(order_id as i64)
    .fetch_optional(c)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// 停止命令确认（BA 应答：Pending/Sent → Acked/Rejected，成败取中性判定），
/// 返回影响行数（0 = 无待确认命令）。
pub async fn ack_stop_command(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
    success: bool,
    result_code: u8,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_stop_command` SET `status` = ?, `result_code` = ?, \
         `next_retry_at` = NULL \
         WHERE `device_id` = ? AND `port_no` = ? AND `status` IN (?, ?)",
    )
    .bind(if success {
        ChargeStopCommandStatus::Acked
    } else {
        ChargeStopCommandStatus::Rejected
    })
    .bind(result_code)
    .bind(device_id)
    .bind(port_no)
    .bind(ChargeStopCommandStatus::Pending)
    .bind(ChargeStopCommandStatus::Sent)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

/// 消费侧定位：设备+端口最近一条未回执停止命令（BB 结束上报处理用）。
pub type UnreportedStopRow = (u64, String, String, u8);

pub async fn find_unreported_stop(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
) -> Result<Option<UnreportedStopRow>, sqlx::Error> {
    let row: Option<UnreportedStopRow> = sqlx::query_as(
        "SELECT `charge_order_id`, `order_no`, `start_command_id`, `port_no` \
         FROM `charge_stop_command` WHERE `device_id` = ? AND `port_no` = ? \
         AND `result_reported` = 0 ORDER BY `created_at` DESC LIMIT 1",
    )
    .bind(device_id)
    .bind(port_no)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 登记停止命令。
#[allow(clippy::too_many_arguments)]
pub async fn insert_stop_command_tx(
    c: &mut MySqlConnection,
    stop_command_id: &str,
    start_command_id: &str,
    order_id: u64,
    order_no: &str,
    user_id: u64,
    device_id: &str,
    port_no: u8,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_stop_command` (`command_id`, `start_command_id`, \
         `charge_order_id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_id`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(stop_command_id)
    .bind(start_command_id)
    .bind(order_id as i64)
    .bind(order_no)
    .bind(user_id as i64)
    .bind(device_id)
    .bind(port_no)
    .bind(order_id as i64)
    .bind(ChargeStopCommandStatus::Pending)
    .execute(c)
    .await?;
    Ok(())
}

/// 启动命令置 stopping（停止链路开始）。
pub async fn mark_command_stopping_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_command` SET `status` = ? WHERE `charge_order_id` = ?")
        .bind(ChargeCommandStatus::Stopping)
        .bind(order_id as i64)
        .execute(c)
        .await?;
    Ok(())
}

/// 停止命令置 sent（下发成功后落库）。
pub async fn mark_stop_command_sent_tx(
    pool: &MySqlPool,
    stop_command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_stop_command` SET `status` = ?, `sent_at` = UTC_TIMESTAMP(3) \
         WHERE `command_id` = ?",
    )
    .bind(ChargeStopCommandStatus::Sent)
    .bind(stop_command_id)
    .execute(pool)
    .await?;
    Ok(())
}
