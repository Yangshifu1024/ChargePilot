//! 超时兜底扫描（旧版 worker 定时循环的状态机出口对应物）。
//! 启动超时：paid 订单的 pending/sent 命令超期未 ack → 订单 refunding
//! （旧版 MarkStartRejectedInTx 同款路径，ADR 002 补充的超时出口）+
//! 登记退款；由定时任务驱动，注入时钟推进可测。SQL 在 repo/sweeper.rs。

use crate::repo::{insert_charge_refund_tx, intent_payment_id_tx, mark_intent_refund_required_tx};
use crate::repo::{set_read_committed, sweeper};
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
    let due = sweeper::due_start_commands(pool, timeout_minutes).await?;
    let mut handled = 0usize;
    for (id, order_no, command_id) in due {
        let reason = "设备启动超时，未收到确认";
        let mut conn = pool.acquire().await?;
        set_read_committed(&mut conn).await?;
        let mut tx = conn.begin().await?;
        // 证据链（票 14）：事务内读取行内状态分派证据，paid → refunding
        // （超时出口与设备拒绝启动同款路径，state-machine.md 迁移 2；failed
        // 保持死值）。行不存在或状态已非 paid 时不迁移不更新——与旧版条件
        // UPDATE 静默 0 行语义一致（updated != 1 跳过该行）。
        let order_status = sweeper::order_status_by_no_tx(&mut tx, id, &order_no).await?;
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
            sweeper::mark_refunding_tx(&mut tx, id, &order_no, reason).await?
        } else {
            0
        };
        if updated == 1 {
            let payment_order_id = intent_payment_id_tx(&mut tx, id).await?;
            // 旧版 Go 切片自动截断，Rust 越界 panic：按实际长度截断。
            let cleaned = command_id.replace('-', "");
            let refund_no = format!("WR{}", &cleaned[..cleaned.len().min(32)]);
            insert_charge_refund_tx(&mut tx, &refund_no, payment_order_id, id, reason).await?;
            sweeper::reject_sweep_command_tx(&mut tx, id, reason).await?;
            // 释放端口与意图（同事务）。
            let device = sweeper::order_device_port_tx(&mut tx, id).await?;
            let _ = chargepilot_mod_order::release_port_tx(&mut tx, &device.0, device.1, id).await;
            mark_intent_refund_required_tx(&mut tx, id).await?;
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
