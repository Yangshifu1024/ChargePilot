//! 启动投递器：消费 charge_payment_confirmed 事件，写启动命令与端口预占，
//! 下发设备命令。幂等（ADR 002）：同订单重复投递不重复建命令。SQL 在 repo/dispatch.rs。

use crate::repo::dispatch;
use crate::sink::CommandSink;
use chargepilot_events::{EnqueuedEvent, EventDispatcher};
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::{Acquire, MySqlPool};

/// 启动命令派发器（事件处理器）。simulation 模式下发送即回执闭环由 sink 侧推进。
pub struct StartDispatcher {
    pub pool: MySqlPool,
    pub allocator: SegmentAllocator,
    pub sink: std::sync::Arc<dyn CommandSink>,
    /// simulation=true：sink 受理即登记成功回执（模拟闭环）；
    /// 真实网关链路等待设备 B8 回执（device_event 处理器推进）。
    pub simulation: bool,
}

impl EventDispatcher for StartDispatcher {
    fn dispatch(
        &self,
        event: &EnqueuedEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send + '_>>
    {
        // 事件载荷复制进 Future：返回值只绑定 self，与 trait 的省略生命期一致。
        let payload = event.payload.clone();
        Box::pin(async move { self.handle(&payload).await })
    }
}

impl StartDispatcher {
    pub async fn handle(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let charge_order_id = payload
            .get("charge_order_id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("事件载荷缺少 charge_order_id"))?;
        // 幂等：已有命令直接确认。
        if dispatch::find_command_tx(&self.pool, charge_order_id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        // 读订单与意图事实（快照与执行量在意图上）。
        let Some((id, order_no, user_id, device_id, port_no, charge_mode, quantity)) =
            dispatch::paid_order_with_intent(&self.pool, charge_order_id).await?
        else {
            // 非 paid 状态（失败/完成）时事件按已处理确认，不再重试。
            return Ok(());
        };
        let port_code = dispatch::order_port_code(&self.pool, id).await?;
        let command_id = chargepilot_platform::ids::uuid_v4();
        let stop_command_id = chargepilot_platform::ids::uuid_v4();

        // 写命令 + 端口预占（同事务；预占冲突即命令重复，幂等跳过）。
        let mut tx = self.pool.begin().await?;
        let conn = tx.acquire().await?;
        let insert_result = dispatch::insert_command_tx(
            conn,
            &command_id,
            &stop_command_id,
            id,
            &order_no,
            user_id,
            &device_id,
            port_no,
            &port_code,
            charge_mode,
            quantity,
        )
        .await;
        match insert_result {
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23000") => {
                // 命令已存在（并发投递）：视为幂等成功。
                tx.rollback().await?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        // 命令落库即标记已发送；设备确认随回执推进（旧版 sent_at 语义）。
        // 端口占用不在受理时写：与旧版一致，成功回执（apply_start_receipt）
        // 才占 active_port_charge，结束回执按订单释放。
        dispatch::mark_command_sent_tx(conn, &command_id).await?;
        tx.commit().await?;

        // 下发设备命令（simulation：直接成功，测试/开发闭环在回执侧推进）。
        let cmd = crate::sink::StartCommand {
            command_id: command_id.clone(),
            charge_order_id: id,
            device_id,
            port_no,
            charge_mode,
            quantity,
        };
        match self.sink.send_start(&cmd).await {
            Ok(()) => {
                // simulation：受理即登记成功回执；真实网关等 B8 回执异步推进。
                if !self.simulation {
                    return Ok(());
                }
                crate::service::receipts::apply_start_receipt(
                    &self.pool,
                    &crate::service::receipts::StartReceiptInput {
                        command_id,
                        charge_order_id: id,
                        order_no,
                        device_id: cmd.device_id.clone(),
                        port_no,
                        port_id: id, // active_port_charge 的复合主键即订单 ID（见 occupy_port_tx）
                        success: true,
                        result_code: 0,
                        occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
                    },
                )
                .await?;
                Ok(())
            }
            Err(e) => {
                // 下发失败：命令标记 rejected，订单走失败路径（可重试由 sweeper 兜底）。
                dispatch::mark_command_rejected_tx(&self.pool, &cmd.command_id, &e).await?;
                anyhow::bail!("启动命令下发失败: {e}")
            }
        }
    }
}
