//! 网关事件汇聚（GatewaySink）：设备注册与事件落库 + 快照更新 + 遥测分发。
//!
//! 分工：高频遥测走有界缓冲批量落库（G2）；业务事件（刷卡/结束/故障等）
//! 与 `domain_event` 同事务写入（事件层 ADR 002），processed_at 立即标记
//! （单体同步处理，联动由事件处理器在批 4/5 接通）。

use std::sync::Arc;

use chargepilot_protocol::{Event, EventType, Registration, SessionRecord, SessionRecorder, Sink};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

use crate::snapshot::SnapshotStore;
use crate::telemetry::{Sample, TelemetryBuffer};

/// 网关汇聚：实现协议 Sink 与会话审计存储。
pub struct GatewaySink {
    /// 数据库连接池。
    pub pool: MySqlPool,
    /// 实时快照表（帧到达先更新）。
    pub snapshots: Arc<SnapshotStore>,
    /// 遥测缓冲。
    pub telemetry: Arc<TelemetryBuffer>,
}

impl GatewaySink {
    /// 组装。
    #[must_use]
    pub fn new(
        pool: MySqlPool,
        snapshots: Arc<SnapshotStore>,
        telemetry: Arc<TelemetryBuffer>,
    ) -> Self {
        Self {
            pool,
            snapshots,
            telemetry,
        }
    }

    /// 事件持久化幂等键：协议 + 设备 + 类型 + 端口 + 接收毫秒。
    fn event_key(event: &Event) -> String {
        let millis = event
            .received_at
            .map(|t| t.timestamp_millis())
            .unwrap_or_default();
        let raw = format!(
            "dc589|{}|{}|{}|{millis}|{}",
            event.device_id,
            event.kind.map(|k| format!("{k:?}")).unwrap_or_default(),
            event.port,
            event.card_number,
        );
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

impl Sink for GatewaySink {
    fn register(
        &self,
        registration: Registration,
    ) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let mut tx = pool.begin().await?;
            // 未导入的设备拒绝注册（device_meta 是导入事实源）。
            let known: Option<u64> = sqlx::query_scalar(
                "SELECT `id` FROM `device_meta` WHERE `device_id` = ? AND `deleted_at` IS NULL",
            )
            .bind(&registration.device_id)
            .fetch_optional(&mut *tx)
            .await?;
            if known.is_none() {
                anyhow::bail!("设备未导入，拒绝注册");
            }
            // device 行按设备号 upsert：首次注册补基础行，此后刷新在线事实。
            sqlx::query(
                "INSERT INTO `device` (`device_id`, `vendor_id`, `firmware_version`, \
                 `last_seen_at`, `last_ip`, `registered_at`) \
                 SELECT ?, COALESCE(dm.`vendor_id`, 0), ?, ?, ?, ? \
                 FROM `device_meta` dm WHERE dm.`device_id` = ? \
                 ON DUPLICATE KEY UPDATE `firmware_version` = VALUES(`firmware_version`), \
                 `last_seen_at` = VALUES(`last_seen_at`), `last_ip` = VALUES(`last_ip`)",
            )
            .bind(&registration.device_id)
            .bind(&registration.software_version)
            .bind(registration.received_at)
            .bind(registration.received_at.to_string())
            .bind(registration.received_at)
            .bind(&registration.device_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn record(&self, event: Event) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        let pool = self.pool.clone();
        let snapshots = self.snapshots.clone();
        let telemetry = self.telemetry.clone();
        Box::pin(async move {
            match event.kind {
                Some(EventType::Heartbeat) => {
                    handle_heartbeat(&pool, &snapshots, &telemetry, &event).await
                }
                Some(EventType::Telemetry) => {
                    // C2 分档：快照 + 缓冲（明细入库，聚合随批）。
                    snapshots.apply_port_telemetry(&event.device_id, &event.charging_ports[0]);
                    let meter = &event.charging_ports[0];
                    telemetry.push(Sample {
                        device_id: event.device_id.clone(),
                        port_no: Some(meter.port),
                        metric: "power_w",
                        value: f64::from(meter.power_deci_watts) / 10.0,
                        ts: event.received_at.unwrap_or_else(chrono::Utc::now),
                    });
                    persist_event(&pool, &event).await
                }
                _ => persist_event(&pool, &event).await,
            }
        })
    }
}

/// 会话审计存储：device_session 行（分区列 created_month = 起始月 1 号）。
impl SessionRecorder for GatewaySink {
    fn record_session(
        &self,
        record: SessionRecord,
    ) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let started = record.started_at.unwrap_or_else(chrono::Utc::now);
            let month = chrono::Utc
                .with_ymd_and_hms(started.year(), started.month(), 1, 0, 0, 0)
                .single()
                .unwrap_or_else(chrono::Utc::now)
                .date_naive();
            // 未知命令集合保留在 SessionRecord 契约中供日志排查，
            // device_session 表不设列（审计表无对应存储）。
            sqlx::query(
                "INSERT INTO `device_session` (`session_id`, `device_id`, `protocol`, \
                 `remote_addr`, `started_at`, `last_active_at`, `ended_at`, `close_reason`, \
                 `bytes_in`, `bytes_out`, `frames_in`, `frames_out`, `created_month`) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&record.session_id)
            .bind(&record.device_id)
            .bind(chargepilot_domain::device::SessionProtocol::Tcp)
            .bind(&record.remote_addr)
            .bind(record.started_at.unwrap_or_else(chrono::Utc::now))
            .bind(record.last_active.unwrap_or_else(chrono::Utc::now))
            .bind(record.ended_at)
            .bind(&record.close_reason)
            .bind(record.bytes_in)
            .bind(record.bytes_out)
            .bind(record.frames_in)
            .bind(record.frames_out)
            .bind(month)
            .execute(&pool)
            .await?;
            Ok(())
        })
    }
}

use chrono::{Datelike as _, TimeZone as _};

/// 心跳处理：快照更新 + 设备/端口在线事实 + 遥测样本入缓冲。
async fn handle_heartbeat(
    pool: &MySqlPool,
    snapshots: &Arc<SnapshotStore>,
    telemetry: &Arc<TelemetryBuffer>,
    event: &Event,
) -> anyhow::Result<()> {
    let has_ports = !event.port_states.is_empty();
    snapshots.apply_heartbeat(
        &event.device_id,
        event.signal,
        event.device_status,
        event.voltage_v,
        event.temperature_c,
        has_ports,
        &event.port_states,
        &event.charging_ports,
    );
    let ts = event.received_at.unwrap_or_else(chrono::Utc::now);
    // 设备级遥测：电压/温度。
    telemetry.push(Sample {
        device_id: event.device_id.clone(),
        port_no: None,
        metric: "voltage_v",
        value: f64::from(event.voltage_v),
        ts,
    });
    telemetry.push(Sample {
        device_id: event.device_id.clone(),
        port_no: None,
        metric: "temperature_c",
        value: f64::from(event.temperature_c),
        ts,
    });
    // 每充电端口：功率与累计电量（0.1 W→W；mWh→kWh）。
    for port in &event.charging_ports {
        telemetry.push(Sample {
            device_id: event.device_id.clone(),
            port_no: Some(port.port),
            metric: "power_w",
            value: f64::from(port.power_deci_watts) / 10.0,
            ts,
        });
        telemetry.push(Sample {
            device_id: event.device_id.clone(),
            port_no: Some(port.port),
            metric: "meter_kwh",
            value: f64::from(port.charged_mwh) / 1_000_000.0,
            ts,
        });
    }
    let mut tx = pool.begin().await?;
    // 设备在线事实（信号按事件时间前进）。
    sqlx::query(
        "UPDATE `device` SET `last_heartbeat_at` = ?, \
         `signal_strength` = ?, `signal_at` = ? WHERE `device_id` = ?",
    )
    .bind(ts)
    .bind(i64::from(event.signal))
    .bind(ts)
    .bind(&event.device_id)
    .execute(&mut *tx)
    .await?;
    // 端口上报状态与最近遥测时间。
    if has_ports {
        for (idx, state) in event.port_states.iter().enumerate() {
            sqlx::query(
                "UPDATE `device_port` SET `reported_status` = ?, `reported_status_at` = ?, \
                 `last_telemetry_at` = ? WHERE `device_id` = ? AND `port_no` = ?",
            )
            .bind(i64::from(*state))
            .bind(ts)
            .bind(ts)
            .bind(&event.device_id)
            .bind(idx as i64 + 1)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    // 心跳晚于故障时判定告警恢复（旧版 AlertSyncService.resolveRecovered 的
    // 事件驱动对应物）：故障消除即置 auto_resolved；失败不影响心跳事实。
    if let Err(e) = resolve_recovered_alerts(pool, event, ts).await {
        tracing::warn!(device = %event.device_id, error = %e, "告警恢复判定失败");
    }
    Ok(())
}

/// 用最新心跳判定该设备活跃告警是否恢复（旧版 worker 周期对账的单体直调形态）。
/// 恢复条件对齐旧版：心跳晚于告警最新故障时间、携带端口状态位（设备级还要求
/// 整机状态存在）、且协议判定故障消除；`auto_resolve` 带旧事件键防误清新故障。
async fn resolve_recovered_alerts(
    pool: &MySqlPool,
    event: &Event,
    heartbeat_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let alerts: Vec<(u64, chrono::NaiveDate, String, String)> = sqlx::query_as(
        "SELECT `id`, `created_month`, `metric`, `event_id` FROM `alert_event` \
         WHERE `device_id` = ? AND `rule_id` IS NULL AND `status` IN (?, ?)",
    )
    .bind(&event.device_id)
    .bind(chargepilot_domain::device::AlertStatus::Active)
    .bind(chargepilot_domain::device::AlertStatus::Acknowledged)
    .fetch_all(pool)
    .await?;
    for (alert_id, created_month, metric, event_key) in alerts {
        // 最新故障时刻取告警来源事件的接收时间；查不到时不判定（缺证据不恢复）。
        let fault_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT `received_at` FROM `device_event` WHERE `event_key` = ?")
                .bind(&event_key)
                .fetch_optional(pool)
                .await?;
        let Some(fault_at) = fault_at else {
            continue;
        };
        if heartbeat_at <= fault_at {
            continue; // 心跳不晚于最新故障：不能作为恢复证据（旧版水印语义）。
        }
        // 无端口状态位的心跳不判恢复（旧版 decodeRecoveryPayload 语义）。
        let recovered = !event.port_states.is_empty()
            && chargepilot_protocol_dc589::fault::fault_recovered(
                &metric,
                event.device_status,
                Some(&event.port_states),
            );
        if recovered {
            chargepilot_mod_device::alert::auto_resolve(
                pool,
                alert_id,
                created_month,
                &event_key,
                heartbeat_at,
            )
            .await?;
        }
    }
    Ok(())
}

/// 业务事件落库：device_event + domain_event 同事务，processed_at 立即标记。
async fn persist_event(pool: &MySqlPool, event: &Event) -> anyhow::Result<()> {
    let key = GatewaySink::event_key(event);
    let payload = serde_json::json!({
        "event_id": event.event_id,
        "event_key": key,
        "device_id": event.device_id,
        "type": event.kind.map(|k| format!("{k:?}")).unwrap_or_default(),
        "port": event.port,
        "result_code": event.result_code,
        "fault_code": event.fault_code,
        "energy_wh": event.energy_wh,
        "power_deci_watts": event.power_deci_watts,
        "stop_reason": event.stop_reason,
        "consumer_type": event.consumer_type,
        "card_number": event.card_number,
        "order_number": event.order_number,
        "charged_seconds": event.charged_seconds,
        "received_at": event.received_at.map(|t| t.to_rfc3339()),
        "started_at": event.started_at.map(|t| t.to_rfc3339()),
        "ended_at": event.ended_at.map(|t| t.to_rfc3339()),
        "amount_cents": event.amount_cents,
        "signal": event.signal,
    });
    let ts = event.received_at.unwrap_or_else(chrono::Utc::now);
    let mut tx = pool.begin().await?;
    // 幂等键冲突（设备重报）静默跳过。
    let inserted = sqlx::query(
        "INSERT IGNORE INTO `device_event` (`event_key`, `protocol_name`, `device_id`, \
         `event_type`, `port_no`, `event_json`, `received_at`, `processed_at`) \
         VALUES (?, 'dc589', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key)
    .bind(&event.device_id)
    .bind(event.kind.map(|k| format!("{k:?}")).unwrap_or_default())
    .bind(i64::from(event.port))
    .bind(&payload)
    .bind(ts)
    .bind(ts)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        // 领域事件同事务入队（进程内投递；处理器联动在批 4/5 登记校验）。
        chargepilot_events::enqueue::in_tx(
            &mut tx,
            chargepilot_events::EventSource::Gateway,
            chargepilot_events::EventKind::DeviceEvent,
            &format!("device-event-{key}"),
            &payload,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
