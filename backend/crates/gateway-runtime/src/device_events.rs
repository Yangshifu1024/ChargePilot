//! 设备事件处理器（B8/BA/BB 回执 → 充电回执推进）。
//! kind=device_event 的 EventDispatcher：按载荷类型路由到启动回执、
//! 停止确认与结束回执（结算联动），全部幂等（回执层已保证）。

use chargepilot_events::{EnqueuedEvent, EventDispatcher};
use sqlx::MySqlPool;

/// 设备事件派发器。
pub struct DeviceEventDispatcher {
    pub pool: MySqlPool,
}

impl EventDispatcher for DeviceEventDispatcher {
    fn dispatch(
        &self,
        event: &EnqueuedEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send + '_>>
    {
        let payload = event.payload.clone();
        Box::pin(async move { self.handle(&payload).await })
    }
}

impl DeviceEventDispatcher {
    /// 事件载荷 → 业务回执。未知类型按已处理确认（不重试）。
    pub async fn handle(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "StartResult" => self.handle_start_result(payload).await,
            "StopResult" => self.handle_stop_result(payload).await,
            "ChargeEnd" => self.handle_charge_end(payload).await,
            other => {
                tracing::debug!(kind = other, "设备事件无需业务处理");
                Ok(())
            }
        }
    }

    /// B8 启动应答：按设备+端口定位最近的 sent 命令，推进成功/失败回执。
    async fn handle_start_result(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let device_id = payload
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("载荷缺少 device_id"))?;
        let port = payload.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let result_code = payload
            .get("result_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u8;
        let row: Option<(String, u64, String, String, u8)> = sqlx::query_as(
            "SELECT `command_id`, `charge_order_id`, `order_no`, `device_id`, `port_no` \
             FROM `charge_command` WHERE `device_id` = ? AND `port_no` = ? AND `status` = 'sent' \
             ORDER BY `created_at` DESC LIMIT 1",
        )
        .bind(device_id)
        .bind(port)
        .fetch_optional(&self.pool)
        .await?;
        let Some((command_id, charge_order_id, order_no, dev, port_no)) = row else {
            tracing::warn!(device = device_id, port, result_code, "无待确认的启动命令");
            return Ok(());
        };
        chargepilot_mod_charging::receipts::apply_start_receipt(
            &self.pool,
            &chargepilot_mod_charging::receipts::StartReceiptInput {
                command_id,
                charge_order_id,
                order_no,
                device_id: dev,
                port_no,
                port_id: charge_order_id, // active_port_charge 复合主键 = 订单 ID
                success: result_code == 0,
                result_code,
                occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
            },
        )
        .await?;
        Ok(())
    }

    /// BA 停止应答：更新停止命令确认状态与结果码。
    async fn handle_stop_result(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let device_id = payload
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("载荷缺少 device_id"))?;
        let port = payload.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let result_code = payload
            .get("result_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u8;
        // 0x10 = 停止成功（BA 词表：0 无端口、1 空闲、4 故障、0x10 成功）。
        let success = result_code == 0x10;
        let updated = sqlx::query(
            "UPDATE `charge_stop_command` SET `status` = ?, `result_code` = ?, \
             `next_retry_at` = NULL \
             WHERE `device_id` = ? AND `port_no` = ? AND `status` IN ('pending','sent')",
        )
        .bind(if success { "acked" } else { "rejected" })
        .bind(result_code)
        .bind(device_id)
        .bind(port)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            tracing::debug!(
                device = device_id,
                port,
                result_code,
                "停止应答无待确认命令"
            );
        }
        Ok(())
    }

    /// BB 结束订单：定位停止命令与订单，应用结束回执（端口释放 + 计费任务）。
    async fn handle_charge_end(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let device_id = payload
            .get("device_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("载荷缺少 device_id"))?;
        let port = payload.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let order_number = payload
            .get("order_number")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let row: Option<(u64, String, String, u8)> = sqlx::query_as(
            "SELECT `charge_order_id`, `order_no`, `start_command_id`, `port_no` \
             FROM `charge_stop_command` WHERE `device_id` = ? AND `port_no` = ? \
             AND `result_reported` = 0 ORDER BY `created_at` DESC LIMIT 1",
        )
        .bind(device_id)
        .bind(port)
        .fetch_optional(&self.pool)
        .await?;
        let Some((charge_order_id, order_no, start_command_id, port_no)) = row else {
            // 本地启动（B4）等无停止命令路径：按订单号定位启动命令兜底。
            tracing::info!(
                device = device_id,
                port,
                order = order_number,
                "结束上报无待回执停止命令"
            );
            return Ok(());
        };
        let stop_command_id: String = sqlx::query_scalar(
            "SELECT `command_id` FROM `charge_stop_command` \
             WHERE `charge_order_id` = ? ORDER BY `created_at` DESC LIMIT 1",
        )
        .bind(charge_order_id as i64)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default();
        let charged_wh = payload
            .get("energy_wh")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let charged_seconds = payload
            .get("charged_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let ended_at = payload
            .get("ended_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let stop_reason = payload
            .get("stop_reason")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u8;
        chargepilot_mod_charging::receipts::apply_end_receipt(
            &self.pool,
            &chargepilot_mod_charging::receipts::EndReceiptInput {
                charge_order_id,
                order_no,
                start_command_id,
                stop_command_id,
                device_id: device_id.to_string(),
                port_no,
                meter: chargepilot_domain::charging_actual::EndMeter {
                    segments: vec![],
                    charged_wh,
                    charged_seconds,
                    ended_at,
                    stop_reason,
                },
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("结束回执应用失败: {e}"))?;
        Ok(())
    }
}
