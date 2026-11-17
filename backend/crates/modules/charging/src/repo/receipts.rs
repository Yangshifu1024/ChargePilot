//! 回执应用的表访问：charge_start_receipt / charge_end_receipt /
//! charge_order / charge_command / charge_billing_job（蓝图第五章 repo 层）。

use chargepilot_domain::charging::command::ChargeCommandStatus;
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::MySqlConnection;

/// 启动回执幂等投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type StartReceiptRow = (u64, Option<String>, Option<String>, Option<u8>, bool);

/// 锁订单并核对设备与端口（user_id 供成功分支写端口占用）。
pub async fn lock_receipt_order_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<(u64, String, u64, String, u8)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `order_no`, `user_id`, `device_id`, `port_no` FROM `charge_order` \
         WHERE `id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 幂等：同命令已登记的启动回执。
pub async fn find_start_receipt_tx(
    c: &mut MySqlConnection,
    command_id: &str,
) -> Result<Option<StartReceiptRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `charge_order_id`, `order_no`, `device_id`, `port_no`, `success` \
         FROM `charge_start_receipt` WHERE `command_id` = ?",
    )
    .bind(command_id)
    .fetch_optional(&mut *c)
    .await
}

/// 读订单状态（uk 幂等分支后校验必须是 paid）。
pub async fn order_status_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<ChargeOrderStatus, sqlx::Error> {
    sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(c)
        .await
}

/// 登记启动回执（uk_command 幂等）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_start_receipt_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    command_id: &str,
    success: bool,
    port_id: u64,
    order_no: &str,
    device_id: &str,
    port_no: u8,
    result_code: u8,
    occurred_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_start_receipt` \
         (`charge_order_id`, `command_id`, `success`, `port_id`, `order_no`, `device_id`, \
          `port_no`, `result_code`, `occurred_at`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order_id as i64)
    .bind(command_id)
    .bind(success)
    .bind(port_id as i64)
    .bind(order_no)
    .bind(device_id)
    .bind(port_no)
    .bind(result_code)
    .bind(occurred_at)
    .execute(c)
    .await?;
    Ok(())
}

/// 启动成功：命令 acked（消费目标状态证据，）。
pub async fn ack_command_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    occurred_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_command` SET `status` = ?, `result_code` = 0, `ack_at` = ?, `result_reported` = 1 WHERE `charge_order_id` = ?")
        .bind(ChargeCommandStatus::Acked)
        .bind(occurred_at)
        .bind(order_id as i64)
        .execute(c)
        .await?;
    Ok(())
}

/// 启动失败：命令 rejected（原因与结果码落库）。
pub async fn reject_command_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    result_code: u8,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_command` SET `status` = ?, `result_code` = ?, `result_reported` = 1, `error` = ? WHERE `charge_order_id` = ?")
        .bind(ChargeCommandStatus::Rejected)
        .bind(result_code)
        .bind(reason)
        .bind(order_id as i64)
        .execute(c)
        .await?;
    Ok(())
}

/// 幂等：已有结束回执的计量 JSON（同内容重放由调用方比对）。
pub async fn find_end_receipt_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(c)
    .await?;
    Ok(row.map(|(json,)| json))
}

/// 成功启动凭据。
pub async fn find_start_credential_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<(String, bool, Option<u64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`command_id` AS CHAR), `success`, `port_id` FROM `charge_start_receipt` \
         WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(c)
    .await
}

/// 登记结束回执证据（计量校验在计费侧，这里先存）。
pub async fn insert_end_receipt_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    stop_command_id: &str,
    meter_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_end_receipt` (`charge_order_id`, `stop_command_id`, `meter_json`) \
         VALUES (?, ?, CAST(? AS JSON))",
    )
    .bind(order_id as i64)
    .bind(stop_command_id)
    .bind(meter_json)
    .execute(c)
    .await?;
    Ok(())
}

/// 登记计费任务（重复订单幂等跳过；供结算批次后续领取）。
pub async fn insert_billing_job_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT IGNORE INTO `charge_billing_job` (`charge_order_id`, `status`) VALUES (?, ?)",
    )
    .bind(order_id as i64)
    .bind(chargepilot_domain::settlement::BillingJobStatus::Pending)
    .execute(c)
    .await?;
    Ok(())
}
