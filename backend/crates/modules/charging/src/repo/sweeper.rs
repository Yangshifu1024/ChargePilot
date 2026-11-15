//! 超时兜底扫描的表访问：charge_command / charge_order /
//! charge_payment_intent（蓝图第五章 repo 层）。

use chargepilot_domain::charging::command::ChargeCommandStatus;
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 到期扫描：paid 订单的 pending/sent 命令超期未 ack（FOR UPDATE 行锁领取）。
pub async fn due_start_commands(
    pool: &MySqlPool,
    timeout_minutes: i64,
) -> Result<Vec<(u64, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.`id`, c.`order_no`, CAST(cmd.`command_id` AS CHAR) \
         FROM `charge_command` cmd \
         JOIN `charge_order` c ON c.`id` = cmd.`charge_order_id` \
         WHERE cmd.`status` IN (?, ?) \
           AND c.`status` = ? \
           AND cmd.`created_at` < DATE_SUB(UTC_TIMESTAMP(3), INTERVAL ? MINUTE) \
         FOR UPDATE",
    )
    .bind(ChargeCommandStatus::Pending)
    .bind(ChargeCommandStatus::Sent)
    .bind(ChargeOrderStatus::Paid)
    .bind(timeout_minutes)
    .fetch_all(pool)
    .await
}

/// 事务内读行内状态（与领取快照区分，RR 下锁不刷新须当前读）。
pub async fn order_status_by_no_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    order_no: &str,
) -> Result<Option<ChargeOrderStatus>, sqlx::Error> {
    sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ? AND `order_no` = ?")
        .bind(order_id as i64)
        .bind(order_no)
        .fetch_optional(c)
        .await
}

/// 条件更新 paid → refunding（updated != 1 跳过该行，与旧版静默 0 行一致）。
pub async fn mark_refunding_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    order_no: &str,
    reason: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_order` SET `status` = ?, `failure_reason` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ?",
    )
    .bind(ChargeOrderStatus::Refunding)
    .bind(reason)
    .bind(order_id as i64)
    .bind(order_no)
    .bind(ChargeOrderStatus::Paid)
    .execute(c)
    .await
    .map(|r| r.rows_affected())
}

/// 超时命令置 rejected（pending/sent 皆收口）。
pub async fn reject_sweep_command_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_command` SET `status` = ?, `error` = ?, `result_reported` = 1 \
         WHERE `charge_order_id` = ? AND `status` IN (?, ?)",
    )
    .bind(ChargeCommandStatus::Rejected)
    .bind(reason)
    .bind(order_id as i64)
    .bind(ChargeCommandStatus::Pending)
    .bind(ChargeCommandStatus::Sent)
    .execute(c)
    .await?;
    Ok(())
}

/// 订单的设备与端口（释放端口预占用）。
pub async fn order_device_port_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<(String, u8), sqlx::Error> {
    sqlx::query_as("SELECT `device_id`, `port_no` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(c)
        .await
}
