//! 设备事件处理器（B8/BA/BB 回执 → 充电回执推进；B6 刷卡 → 准入与授权/拒绝；
//! C0 故障 → 告警生成；B4 本地启动 → 平台侧补单）。
//! kind=device_event 的 EventDispatcher：按载荷类型路由，全部幂等——
//! 回执层已保证；刷卡走 card_operation 的 uk_device_event 幂等恢复；
//! 告警按事件键重放跳过；补单与刷卡共用同一准入事务。
//! 帧下发经会话注册表：设备离线即失败，由事件层退避重试（处理器不 panic）。

use std::sync::Arc;

use chargepilot_events::{EnqueuedEvent, EventDispatcher};
use chargepilot_mod_card::SwipeError;
use chargepilot_mod_card::swipe::{SwipeInput, SwipeService};
use chargepilot_mod_payment::scan::TablePortScanner;
use chargepilot_platform::ids::SegmentAllocator;
use chargepilot_protocol::event_payload::{
    CardEventPayload, ChargeEndPayload, CommandResultPayload,
};
use chargepilot_protocol::{Command, CommandKind, EventType};
use sqlx::MySqlPool;

/// 设备事件派发器。
pub struct DeviceEventDispatcher {
    pub pool: MySqlPool,
    /// 刷卡准入事务的号段分配器（订单/支付单编号）。
    pub allocator: SegmentAllocator,
    /// 会话注册表：授权/拒绝/余额帧经设备连接下发。
    pub registry: Arc<dyn chargepilot_protocol::SessionRouter>,
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

/// 载荷按类别解析为读取视图；解析失败打警告后按已处理确认——坏载荷重试
/// 也不会成功，交由状态机兜底扫描接管（不再静默补 0，也不再无限重试）。
fn parse_payload<T: serde::de::DeserializeOwned>(
    payload: &serde_json::Value,
    kind: EventType,
) -> Option<T> {
    match serde_json::from_value::<T>(payload.clone()) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            tracing::warn!(kind = kind.as_str(), error = %e, "设备事件载荷解析失败，跳过");
            None
        }
    }
}

impl DeviceEventDispatcher {
    /// 事件载荷 → 业务回执。按词表路由（未知类型按已处理确认，不重试）。
    pub async fn handle(&self, payload: &serde_json::Value) -> anyhow::Result<()> {
        let kind = payload
            .get("type")
            .and_then(|v| v.as_str())
            .and_then(EventType::parse);
        let Some(kind) = kind else {
            let raw = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            tracing::debug!(kind = raw, "设备事件无需业务处理");
            return Ok(());
        };
        match kind {
            EventType::StartResult => {
                if let Some(p) = parse_payload::<CommandResultPayload>(payload, kind) {
                    self.handle_start_result(&p).await?;
                }
            }
            EventType::StopResult => {
                if let Some(p) = parse_payload::<CommandResultPayload>(payload, kind) {
                    self.handle_stop_result(&p).await?;
                }
            }
            EventType::ChargeEnd => {
                if let Some(p) = parse_payload::<ChargeEndPayload>(payload, kind) {
                    self.handle_charge_end(&p).await?;
                }
            }
            EventType::CardSwipe => {
                if let Some(p) = parse_payload::<CardEventPayload>(payload, kind) {
                    self.handle_card_swipe(&p).await?;
                }
            }
            EventType::CardBalanceQuery => {
                if let Some(p) = parse_payload::<CardEventPayload>(payload, kind) {
                    self.handle_card_balance(&p).await?;
                }
            }
            EventType::Fault => {
                if let Some(p) = parse_payload::<CardEventPayload>(payload, kind) {
                    self.handle_fault(&p).await?;
                }
            }
            EventType::LocalStart => {
                if let Some(p) = parse_payload::<CardEventPayload>(payload, kind) {
                    self.handle_local_start(&p).await?;
                }
            }
            other @ (EventType::Heartbeat
            | EventType::RemoteResult
            | EventType::ConfigResult
            | EventType::Telemetry
            | EventType::TimeSync) => {
                tracing::debug!(kind = other.as_str(), "设备事件无需业务处理");
            }
        }
        Ok(())
    }

    /// B8 启动应答：按设备+端口定位最近的 sent 命令，推进成功/失败回执。
    async fn handle_start_result(&self, p: &CommandResultPayload) -> anyhow::Result<()> {
        // success 由协议适配器翻译（厂商应答码不进运行时）；旧格式在途载荷
        // 无此字段，按证据缺失跳过，由状态机兜底扫描接管。
        let Some(success) = p.success else {
            tracing::warn!(
                device = %p.device_id,
                port = p.port,
                result_code = p.result_code,
                "启动应答缺中性成败判定（旧格式在途载荷），跳过"
            );
            return Ok(());
        };
        let row: Option<(String, u64, String, String, u8)> = sqlx::query_as(
            "SELECT CAST(`command_id` AS CHAR), `charge_order_id`, `order_no`, `device_id`, `port_no` \
             FROM `charge_command` WHERE `device_id` = ? AND `port_no` = ? AND `status` = ? \
             ORDER BY `created_at` DESC LIMIT 1",
        )
        .bind(&p.device_id)
        .bind(p.port)
        .bind(chargepilot_domain::charging::command::ChargeCommandStatus::Sent)
        .fetch_optional(&self.pool)
        .await?;
        let Some((command_id, charge_order_id, order_no, dev, port_no)) = row else {
            tracing::warn!(device = %p.device_id, port = p.port, result_code = p.result_code, "无待确认的启动命令");
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
                success,
                result_code: p.result_code,
                occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
            },
        )
        .await?;
        Ok(())
    }

    /// BA 停止应答：更新停止命令确认状态与结果码（成败取中性判定字段）。
    async fn handle_stop_result(&self, p: &CommandResultPayload) -> anyhow::Result<()> {
        let Some(success) = p.success else {
            tracing::warn!(
                device = %p.device_id,
                port = p.port,
                result_code = p.result_code,
                "停止应答缺中性成败判定（旧格式在途载荷），跳过"
            );
            return Ok(());
        };
        let updated = sqlx::query(
            "UPDATE `charge_stop_command` SET `status` = ?, `result_code` = ?, \
             `next_retry_at` = NULL \
             WHERE `device_id` = ? AND `port_no` = ? AND `status` IN (?, ?)",
        )
        .bind(if success {
            chargepilot_domain::charging::command::ChargeStopCommandStatus::Acked
        } else {
            chargepilot_domain::charging::command::ChargeStopCommandStatus::Rejected
        })
        .bind(p.result_code)
        .bind(&p.device_id)
        .bind(p.port)
        .bind(chargepilot_domain::charging::command::ChargeStopCommandStatus::Pending)
        .bind(chargepilot_domain::charging::command::ChargeStopCommandStatus::Sent)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            tracing::debug!(
                device = %p.device_id,
                port = p.port,
                result_code = p.result_code,
                "停止应答无待确认命令"
            );
        }
        Ok(())
    }

    /// BB 结束订单：定位停止命令与订单，应用结束回执（端口释放 + 计费任务）。
    async fn handle_charge_end(&self, p: &ChargeEndPayload) -> anyhow::Result<()> {
        let row: Option<(u64, String, String, u8)> = sqlx::query_as(
            "SELECT `charge_order_id`, `order_no`, `start_command_id`, `port_no` \
             FROM `charge_stop_command` WHERE `device_id` = ? AND `port_no` = ? \
             AND `result_reported` = 0 ORDER BY `created_at` DESC LIMIT 1",
        )
        .bind(&p.device_id)
        .bind(p.port)
        .fetch_optional(&self.pool)
        .await?;
        let Some((charge_order_id, order_no, start_command_id, port_no)) = row else {
            // 本地启动（B4）等无停止命令路径：按订单号定位启动命令兜底。
            tracing::info!(
                device = %p.device_id,
                port = p.port,
                order = %p.order_number,
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
        chargepilot_mod_charging::receipts::apply_end_receipt(
            &self.pool,
            &chargepilot_mod_charging::receipts::EndReceiptInput {
                charge_order_id,
                order_no,
                start_command_id,
                stop_command_id,
                device_id: p.device_id.clone(),
                port_no,
                meter: chargepilot_domain::charging::actual::EndMeter {
                    segments: vec![],
                    charged_wh: p.energy_wh,
                    charged_seconds: p.charged_seconds,
                    ended_at: p.ended_at.unwrap_or_else(chrono::Utc::now),
                    stop_reason: p.stop_reason,
                },
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("结束回执应用失败: {e}"))?;
        Ok(())
    }

    /// B6 在线卡刷卡：card 域准入（幂等扣款事务）→ 授权/拒绝帧。
    /// 受理成功不发卡回复——启动授权由 charge_payment_confirmed 出站事件
    /// 经 StartDispatcher 下发 B7（旧版 CardDispatcher.decide 语义）；
    /// 明确拒绝下发 BD（余额不足保留余额显示，其余置无效卡）；
    /// 其他失败返回 Err 交事件层退避重试（受理幂等，重放不重复扣款）。
    async fn handle_card_swipe(&self, f: &CardEventPayload) -> anyhow::Result<()> {
        if f.card_number == 0 {
            tracing::warn!(device = %f.device_id, port = f.port, "刷卡事件缺少卡号");
            return Ok(());
        }
        self.swipe_and_reply(f).await
    }

    /// D0 卡余额查询：查卡与钱包可用余额后下发 D1；零卡号按无效卡应答。
    async fn handle_card_balance(&self, f: &CardEventPayload) -> anyhow::Result<()> {
        let mut invalid = f.card_number == 0;
        let mut units = 0u16;
        if !invalid {
            let (card_valid, balance_cents) = card_balance_facts(&self.pool, f).await?;
            invalid = !card_valid;
            units = chargepilot_protocol_dc589::cards::card_balance_units(balance_cents);
        }
        self.send_to_device(
            &f.device_id,
            Command {
                kind: CommandKind::CardBalance,
                port: f.port,
                session_id: [0; 6],
                order_bcd: [0; 8],
                mode: 0,
                quantity: 0,
                consumer_type: 0,
                card_number: f.card_number,
                card_balance_units: units,
                card_invalid: invalid,
            },
        )
        .await
    }

    /// C0 故障上报：协议故障码 → 中性指标名后写告警域（生成/合并升级/重放跳过
    /// 均由 record_fault 事务保证）。FaultCode=0 按旧版语义忽略。
    async fn handle_fault(&self, f: &CardEventPayload) -> anyhow::Result<()> {
        if f.fault_code == 0 {
            tracing::debug!(device = %f.device_id, port = f.port, "故障码为零，忽略");
            return Ok(());
        }
        let received_at = f.received_at.unwrap_or_else(chrono::Utc::now);
        let metric = chargepilot_protocol_dc589::fault::fault_metric(f.port, f.fault_code);
        chargepilot_mod_device::alert::record_fault(
            &self.pool,
            &f.device_id,
            &f.event_key,
            &metric.metric,
            metric.severity.as_str(),
            i64::from(f.fault_code),
            received_at,
        )
        .await
        .map_err(|e| anyhow::anyhow!("告警写入失败: {e}"))
    }

    /// B4 本地启动上报：投币/免费启动不涉平台卡与资金，事件留痕即可；
    /// 离线卡本地启动（消费类型 1）平台侧补校验与补单——复用刷卡准入事务
    /// （卡有效性、额度、扣款、订单与操作记录同事务，uk_device_event 幂等），
    /// 校验拒绝按刷卡链下发 BD。B5 收到确认由适配器落库成功后下发。
    async fn handle_local_start(&self, f: &CardEventPayload) -> anyhow::Result<()> {
        match (f.consumer_type, f.card_number) {
            (1, card) if card != 0 => self.swipe_and_reply(f).await,
            (0, _) | (4, _) => {
                tracing::debug!(
                    device = %f.device_id,
                    port = f.port,
                    consumer = f.consumer_type,
                    order = %f.order_number,
                    "本地启动不涉平台卡，仅事件留痕"
                );
                Ok(())
            }
            (consumer, card) => {
                tracing::warn!(
                    device = %f.device_id,
                    port = f.port,
                    consumer,
                    card,
                    "本地启动消费类型未知，不补单"
                );
                Ok(())
            }
        }
    }

    /// 刷卡准入与拒绝应答（B6 与 B4 离线卡共用）。
    async fn swipe_and_reply(&self, f: &CardEventPayload) -> anyhow::Result<()> {
        if f.port == 0xFF {
            // 端口号错误标志：直接按无效卡拒绝（旧版 decide 语义）。
            return self
                .send_to_device(
                    &f.device_id,
                    Command {
                        kind: CommandKind::CardDenied,
                        port: f.port,
                        session_id: [0; 6],
                        order_bcd: [0; 8],
                        mode: 0,
                        quantity: 0,
                        consumer_type: 0,
                        card_number: f.card_number,
                        card_balance_units: 0,
                        card_invalid: true,
                    },
                )
                .await;
        }
        // 设备端口号 → 业务端口编码；未开通视为暂不可处理（重试）。
        let Some(port_code) = resolve_port_code(&self.pool, &f.device_id, f.port).await else {
            anyhow::bail!("端口未开通: device={} port={}", f.device_id, f.port);
        };
        let service = SwipeService {
            pool: self.pool.clone(),
            allocator: self.allocator.clone(),
            scanner: Arc::new(TablePortScanner::new(self.pool.clone())),
        };
        let input = SwipeInput {
            card_no: f.card_number.to_string(),
            event_id: f.event_id.clone(),
            port_id: port_code,
            occurred_at_unix_micros: f
                .received_at
                .map(|t| t.timestamp_micros())
                .unwrap_or_default(),
        };
        match service.apply(&input).await {
            Ok(result) => {
                tracing::info!(
                    device = %f.device_id,
                    card = f.card_number,
                    order = result.charge_order_id,
                    kind = %result.kind,
                    replayed = result.replayed,
                    "刷卡受理"
                );
                Ok(())
            }
            Err(e) if e.is_declined() => {
                // 明确拒绝：余额不足保留余额显示，其余按无效卡拒绝。
                let balance_cents = card_balance_facts(&self.pool, f)
                    .await
                    .map(|(_, cents)| cents)
                    .unwrap_or(0);
                tracing::info!(
                    device = %f.device_id,
                    card = f.card_number,
                    reason = %e,
                    "刷卡拒绝"
                );
                self.send_to_device(
                    &f.device_id,
                    Command {
                        kind: CommandKind::CardDenied,
                        port: f.port,
                        session_id: [0; 6],
                        order_bcd: [0; 8],
                        mode: 0,
                        quantity: 0,
                        consumer_type: 0,
                        card_number: f.card_number,
                        card_balance_units: chargepilot_protocol_dc589::cards::card_balance_units(
                            balance_cents,
                        ),
                        card_invalid: e != SwipeError::CardBalance,
                    },
                )
                .await
            }
            Err(e) => Err(anyhow::anyhow!("刷卡准入未决: {e}")),
        }
    }

    /// 经会话注册表向设备下发命令；设备离线返回错误（事件层退避重试）。
    async fn send_to_device(&self, device_id: &str, command: Command) -> anyhow::Result<()> {
        let session = self.registry.get(device_id).await;
        let Some(session) = session else {
            anyhow::bail!("设备离线: {device_id}");
        };
        session
            .send(command)
            .await
            .map_err(|e| anyhow::anyhow!("设备命令下发失败: {e}"))
    }
}

/// 按设备号与端口号解析业务端口编码（device_port 事实表）。
async fn resolve_port_code(pool: &MySqlPool, device_id: &str, port_no: u8) -> Option<String> {
    sqlx::query_scalar(
        "SELECT `port_code` FROM `device_port` \
         WHERE `device_id` = ? AND `port_no` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(device_id)
    .bind(i64::from(port_no))
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// 卡有效性事实与钱包可用余额（分）：卡缺失/停用返回 (false, 0)。
async fn card_balance_facts(pool: &MySqlPool, f: &CardEventPayload) -> anyhow::Result<(bool, i64)> {
    let row: Option<(chargepilot_domain::card::CardStatus, Option<i64>)> = sqlx::query_as(
        "SELECT c.`status`, w.`balance_cents` - w.`frozen_cents` \
         FROM `online_card` c \
         LEFT JOIN `wallet_account` w ON w.`user_id` = c.`user_id` AND w.`deleted_at` IS NULL \
         WHERE c.`card_no` = ? LIMIT 1",
    )
    .bind(f.card_number.to_string())
    .fetch_optional(pool)
    .await?;
    let Some((status, available)) = row else {
        return Ok((false, 0));
    };
    Ok((
        status == chargepilot_domain::card::CardStatus::Active,
        available.unwrap_or(0),
    ))
}
