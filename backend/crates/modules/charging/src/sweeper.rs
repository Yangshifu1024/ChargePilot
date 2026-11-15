//! 超时兜底扫描（旧版 worker 定时循环的状态机出口对应物）。
//! 启动超时：paid 订单的 pending/sent 命令超期未 ack → 订单 refunding
//! （旧版 MarkStartRejectedInTx 同款路径，ADR 002 补充的超时出口）+
//! 登记退款；由定时任务驱动，注入时钟推进可测。

use sqlx::{Acquire, MySqlPool};

/// 兜底扫描器。
pub struct StartTimeoutSweeper {
    pub pool: MySqlPool,
    /// 启动超时阈值（分钟；旧版 paid 未起充的兜底出口）。
    pub timeout_minutes: i64,
}

/// 扫描一轮：把超期未确认的启动命令对应订单置 refunding 并登记退款。
/// 返回处理单数。命令 sent 超期视为设备无应答；退款成功后由执行器
/// 收尾 refunding → refunded。
pub async fn sweep_once(pool: &MySqlPool, timeout_minutes: i64) -> anyhow::Result<usize> {
    let due: Vec<(u64, String, String)> = sqlx::query_as(
        "SELECT c.`id`, c.`order_no`, CAST(cmd.`command_id` AS CHAR) \
         FROM `charge_command` cmd \
         JOIN `charge_order` c ON c.`id` = cmd.`charge_order_id` \
         WHERE cmd.`status` IN (?, ?) \
           AND c.`status` = ? \
           AND cmd.`created_at` < DATE_SUB(UTC_TIMESTAMP(3), INTERVAL ? MINUTE) \
         FOR UPDATE",
    )
    .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Pending)
    .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Sent)
    .bind(chargepilot_domain::order::ChargeOrderStatus::Paid)
    .bind(timeout_minutes)
    .fetch_all(pool)
    .await?;
    let mut handled = 0usize;
    for (id, order_no, command_id) in due {
        let reason = "设备启动超时，未收到确认";
        let mut conn = pool.acquire().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        // 证据链（票 14）：事务内读取行内状态分派证据，paid → refunding
        // （超时出口与设备拒绝启动同款路径，state-machine.md 迁移 2；failed
        // 保持死值）。行不存在或状态已非 paid 时不迁移不更新——与旧版条件
        // UPDATE 静默 0 行语义一致（updated != 1 跳过该行）。
        let order_status: Option<chargepilot_domain::order::ChargeOrderStatus> =
            sqlx::query_scalar(
                "SELECT `status` FROM `charge_order` WHERE `id` = ? AND `order_no` = ?",
            )
            .bind(id as i64)
            .bind(&order_no)
            .fetch_optional(&mut *tx)
            .await?;
        let paid = order_status
            .and_then(|st| {
                chargepilot_domain::charging::state::evidence_of(
                    chargepilot_domain::charging::state::OrderId(id),
                    st,
                )
            })
            .and_then(chargepilot_domain::charging::state::StateEvidence::into_paid);
        let updated = if let Some(evidence) = paid {
            let _refunding = evidence.mark_start_rejected(reason);
            sqlx::query(
                "UPDATE `charge_order` SET `status` = ?, `failure_reason` = ? \
                 WHERE `id` = ? AND `order_no` = ? AND `status` = ?",
            )
            .bind(chargepilot_domain::order::ChargeOrderStatus::Refunding)
            .bind(reason)
            .bind(id as i64)
            .bind(&order_no)
            .bind(chargepilot_domain::order::ChargeOrderStatus::Paid)
            .execute(&mut *tx)
            .await?
            .rows_affected()
        } else {
            0
        };
        if updated == 1 {
            let payment_order_id: u64 = sqlx::query_scalar(
                "SELECT `payment_order_id` FROM `charge_payment_intent` WHERE `charge_order_id` = ?",
            )
            .bind(id as i64)
            .fetch_one(&mut *tx)
            .await?;
            // 旧版 Go 切片自动截断，Rust 越界 panic：按实际长度截断。
            let cleaned = command_id.replace('-', "");
            let refund_no = format!("WR{}", &cleaned[..cleaned.len().min(32)]);
            sqlx::query(
                "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
                 `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
                 SELECT ?, ?, c.`user_id`, 'charge', c.`id`, p.`paid_cents`, ?, ?, ?, c.`created_month` \
                 FROM `charge_order` c JOIN `payment_order` p ON p.`id` = ? WHERE c.`id` = ?",
            )
            .bind(&refund_no)
            .bind(payment_order_id as i64)
            .bind(reason)
            .bind(chargepilot_domain::money::RefundStatus::Pending)
            .bind(chargepilot_domain::money::ExecutionPolicy::Automatic)
            .bind(payment_order_id as i64)
            .bind(id as i64)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE `charge_command` SET `status` = ?, `error` = ?, `result_reported` = 1 \
                 WHERE `charge_order_id` = ? AND `status` IN (?, ?)",
            )
            .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Rejected)
            .bind(reason)
            .bind(id as i64)
            .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Pending)
            .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Sent)
            .execute(&mut *tx)
            .await?;
            // 释放端口与意图（同事务）。
            let device: (String, u8) =
                sqlx::query_as("SELECT `device_id`, `port_no` FROM `charge_order` WHERE `id` = ?")
                    .bind(id as i64)
                    .fetch_one(&mut *tx)
                    .await?;
            let _ = chargepilot_mod_order::release_port_tx(&mut tx, &device.0, device.1, id).await;
            sqlx::query(
                "UPDATE `charge_payment_intent` SET `status` = ? \
                 WHERE `charge_order_id` = ? AND `status` = ?",
            )
            .bind(chargepilot_domain::payment::PaymentIntentStatus::RefundRequired)
            .bind(id as i64)
            .bind(chargepilot_domain::payment::PaymentIntentStatus::Paid)
            .execute(&mut *tx)
            .await?;
            let cleaned = command_id.replace('-', "");
            let event_id = format!("T{}", &cleaned[..cleaned.len().min(32)]);
            chargepilot_mod_order::append_domain_event_tx(
                &mut tx,
                &event_id,
                "gateway",
                "charge_start_timeout",
                &serde_json::json!({
                    "event_id": event_id,
                    "charge_order_id": id,
                    "order_no": order_no,
                    "payment_order_id": payment_order_id,
                }),
            )
            .await?;
            tx.commit().await?;
            handled += 1;
        } else {
            tx.rollback().await?;
        }
    }
    Ok(handled)
}
