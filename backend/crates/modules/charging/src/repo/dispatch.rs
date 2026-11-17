//! 启动投递的表访问：charge_command / charge_order /
//! charge_payment_intent（蓝图第五章 repo 层）。

use chargepilot_domain::charging::command::ChargeCommandStatus;
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 幂等：同订单已有启动命令。
pub async fn find_command_tx(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`command_id` AS CHAR) FROM `charge_command` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// 消费侧定位：设备+端口最近一条 sent 启动命令（B8 应答处理用）。
pub type SentCommandRow = (String, u64, String, String, u8);

pub async fn find_latest_sent_command(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
) -> Result<Option<SentCommandRow>, sqlx::Error> {
    let row: Option<SentCommandRow> = sqlx::query_as(
        "SELECT CAST(`command_id` AS CHAR), `charge_order_id`, `order_no`, `device_id`, `port_no` \
         FROM `charge_command` WHERE `device_id` = ? AND `port_no` = ? AND `status` = ? \
         ORDER BY `created_at` DESC LIMIT 1",
    )
    .bind(device_id)
    .bind(port_no)
    .bind(ChargeCommandStatus::Sent)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 读订单与意图事实（快照与执行量在意图上）。
pub async fn paid_order_with_intent(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<(u64, String, u64, String, u8, u8, u16)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.`id`, c.`order_no`, c.`user_id`, c.`device_id`, c.`port_no`, \
                i.`charge_mode`, i.`charge_quantity` \
         FROM `charge_order` c JOIN `charge_payment_intent` i ON i.`charge_order_id` = c.`id` \
         WHERE c.`id` = ? AND c.`status` = ? LIMIT 1",
    )
    .bind(charge_order_id as i64)
    .bind(ChargeOrderStatus::Paid)
    .fetch_optional(pool)
    .await
}

/// 订单的端口编码。
pub async fn order_port_code(pool: &MySqlPool, order_id: u64) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT `port_code` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(pool)
        .await
}

/// 写启动命令（预占冲突即命令重复，23000 原样上抛由调用方判定幂等）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_command_tx(
    conn: &mut MySqlConnection,
    command_id: &str,
    stop_command_id: &str,
    order_id: u64,
    order_no: &str,
    user_id: u64,
    device_id: &str,
    port_no: u8,
    port_code: &str,
    charge_mode: u8,
    quantity: u16,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_command` \
         (`command_id`, `stop_command_id`, `charge_order_id`, `payment_order_id`, `order_no`, \
          `user_id`, `device_id`, `port_no`, `port_code`, `owns_port`, `status`, \
          `charge_mode`, `quantity`) \
         SELECT ?, ?, ?, p.`payment_order_id`, ?, ?, ?, ?, ?, 1, ?, ?, ? \
         FROM `charge_payment_intent` p WHERE p.`charge_order_id` = ?",
    )
    .bind(command_id)
    .bind(stop_command_id)
    .bind(order_id as i64)
    .bind(order_no)
    .bind(user_id as i64)
    .bind(device_id)
    .bind(port_no)
    .bind(port_code)
    .bind(ChargeCommandStatus::Pending)
    .bind(charge_mode)
    .bind(quantity as i64)
    .bind(order_id as i64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 命令落库即标记已发送；设备确认随回执推进。
pub async fn mark_command_sent_tx(
    conn: &mut MySqlConnection,
    command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_command` SET `status` = ?, `sent_at` = UTC_TIMESTAMP(3) WHERE `command_id` = ?")
        .bind(ChargeCommandStatus::Sent)
        .bind(command_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// 下发失败：命令标记 rejected（订单失败路径由 sweeper 兜底）。
pub async fn mark_command_rejected_tx(
    pool: &MySqlPool,
    command_id: &str,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_command` SET `status` = ?, `error` = ? WHERE `command_id` = ?")
        .bind(ChargeCommandStatus::Rejected)
        .bind(error)
        .bind(command_id)
        .execute(pool)
        .await?;
    Ok(())
}
