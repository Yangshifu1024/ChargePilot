//! 启动投递器：消费 charge_payment_confirmed 事件，写启动命令与端口预占，
//! 下发设备命令。幂等（ADR 002）：同订单重复投递不重复建命令。

use crate::sink::CommandSink;
use chargepilot_events::{EnqueuedEvent, EventDispatcher};
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::{Acquire, MySqlPool};

/// 启动命令派发器（事件处理器）。simulation 模式下发送即回执闭环由 sink 侧推进。
pub struct StartDispatcher {
    pub pool: MySqlPool,
    pub allocator: SegmentAllocator,
    pub sink: std::sync::Arc<dyn CommandSink>,
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
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT CAST(`command_id` AS CHAR) FROM `charge_command` WHERE `charge_order_id` = ?",
        )
        .bind(charge_order_id as i64)
        .fetch_optional(&self.pool)
        .await?;
        if existing.is_some() {
            return Ok(());
        }
        // 读订单与意图事实（快照与执行量在意图上）。
        let order: Option<(u64, String, u64, String, u8, u8, u16)> = sqlx::query_as(
            "SELECT c.`id`, c.`order_no`, c.`user_id`, c.`device_id`, c.`port_no`, \
                    i.`charge_mode`, i.`charge_quantity` \
             FROM `charge_order` c JOIN `charge_payment_intent` i ON i.`charge_order_id` = c.`id` \
             WHERE c.`id` = ? AND c.`status` = ? LIMIT 1",
        )
        .bind(charge_order_id as i64)
        .bind(chargepilot_mod_order::OrderStatus::PAID)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, order_no, user_id, device_id, port_no, charge_mode, quantity)) = order else {
            // 非 paid 状态（失败/完成）时事件按已处理确认，不再重试。
            return Ok(());
        };
        let port_code: String =
            sqlx::query_scalar("SELECT `port_code` FROM `charge_order` WHERE `id` = ?")
                .bind(id as i64)
                .fetch_one(&self.pool)
                .await?;
        let command_id = chargepilot_platform::ids::uuid_v4();
        let stop_command_id = chargepilot_platform::ids::uuid_v4();

        // 写命令 + 端口预占（同事务；预占冲突即命令重复，幂等跳过）。
        let mut tx = self.pool.begin().await?;
        let conn = tx.acquire().await?;
        let inserted = sqlx::query(
            "INSERT INTO `charge_command` \
             (`command_id`, `stop_command_id`, `charge_order_id`, `payment_order_id`, `order_no`, \
              `user_id`, `device_id`, `port_no`, `port_code`, `owns_port`, `status`, \
              `charge_mode`, `quantity`) \
             SELECT ?, ?, ?, p.`payment_order_id`, ?, ?, ?, ?, ?, 1, 'pending', ?, ? \
             FROM `charge_payment_intent` p WHERE p.`charge_order_id` = ?",
        )
        .bind(&command_id)
        .bind(&stop_command_id)
        .bind(id as i64)
        .bind(&order_no)
        .bind(user_id as i64)
        .bind(&device_id)
        .bind(port_no)
        .bind(&port_code)
        .bind(charge_mode)
        .bind(quantity as i64)
        .bind(id as i64)
        .execute(&mut *conn)
        .await;
        match inserted {
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23000") => {
                // 命令已存在（并发投递）：视为幂等成功。
                tx.rollback().await?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        chargepilot_mod_order::occupy_port_tx(
            &mut *conn,
            &device_id,
            port_no,
            id,
            user_id,
            chrono::Utc::now(),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // 命令落库即标记已发送；设备确认随回执推进（旧版 sent_at 语义）。
        sqlx::query("UPDATE `charge_command` SET `status` = 'sent', `sent_at` = UTC_TIMESTAMP(3) WHERE `command_id` = ?")
            .bind(&command_id)
            .execute(&mut *conn)
            .await?;
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
                // 已受理（ack）：模拟链路立即登记成功回执，真实网关回执异步到达。
                crate::receipts::apply_start_receipt(
                    &self.pool,
                    &crate::receipts::StartReceiptInput {
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
                sqlx::query("UPDATE `charge_command` SET `status` = 'rejected', `error` = ? WHERE `command_id` = ?")
                    .bind(&e)
                    .bind(&cmd.command_id)
                    .execute(&self.pool)
                    .await?;
                anyhow::bail!("启动命令下发失败: {e}")
            }
        }
    }
}
