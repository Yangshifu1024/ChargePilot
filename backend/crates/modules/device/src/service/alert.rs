//! 告警用例：设备故障事件 → 告警生成/合并/恢复判定（心跳水位防乱序重放）+
//! 后台列表/认领。同步入口（record_fault / auto_resolve）由设备事件链调用。
//! SQL 在 repo/alert.rs。

use crate::repo::alert;
use crate::{DeviceError, StationResult};
use chargepilot_domain::device::AlertStatus;
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use chrono::Datelike as _;
use serde_json::{Value, json};
use sqlx::MySqlPool;

pub use crate::repo::alert::AlertRow;

/// 记录一条设备故障事件为告警：
/// - 证据核对（device_id 一致、FaultCode=0 忽略）
/// - 同维度最新告警 FOR UPDATE：同 event_id 重放 / 乱序旧事件 / 已恢复后旧事件 → 跳过
/// - active/acknowledged → 合并升级（仅更新 event_id 与 value，保留首次时间）
/// - 否则插入新 active 告警
pub async fn record_fault(
    pool: &MySqlPool,
    device_id: &str,
    event_id: &str,
    metric: &str,
    severity: &str,
    value: i64,
    received_at: chrono::DateTime<chrono::Utc>,
) -> StationResult<()> {
    let mut tx = pool.begin().await?;
    let existing = alert::latest_device_metric_tx(&mut tx, device_id, metric).await?;
    if let Some((id, prev_event, status, created_at, resolved_at)) = existing {
        if prev_event == event_id {
            tx.commit().await?;
            return Ok(()); // 重放
        }
        let older_than_creation = created_at
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .is_some_and(|t| received_at < t.with_timezone(&chrono::Utc));
        if older_than_creation {
            tx.commit().await?;
            return Ok(()); // 乱序旧事件
        }
        if let Some(r) = resolved_at
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            && received_at <= r
        {
            tx.commit().await?;
            return Ok(()); // 已恢复后的旧事件
        }
        if status == AlertStatus::Active || status == AlertStatus::Acknowledged {
            // 升级合并：保留首次 created_at
            alert::upgrade_alert_tx(&mut tx, id, event_id, value).await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    // 新告警
    let month = match chrono::Utc::now().date_naive().with_day(1) {
        Some(d) => d,
        None => panic!("月首日必然存在"),
    };
    alert::insert_alert_tx(
        &mut tx,
        device_id,
        severity,
        metric,
        value,
        event_id,
        received_at,
        month,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub use crate::repo::alert::auto_resolve;

/// 后台告警列表（倒序，按设备站点+厂商范围——范围本轮全量）。
pub async fn list(pool: &MySqlPool) -> StationResult<Vec<AlertRow>> {
    alert::list_active_rows(pool).await
}

/// 认领：锁行 + 重放静默 + 非 active 409 + 写 acknowledged + 审计。
pub async fn ack(
    pool: &MySqlPool,
    actor: &Profile,
    alert_id: u64,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::ALERT_ACK)
    {
        return Err(DeviceError::Forbidden);
    }
    let (id, _created_month, status) = alert::lock_alert_tx(&mut tx, alert_id)
        .await?
        .ok_or(DeviceError::NotFound)?;
    if status == AlertStatus::Acknowledged {
        tx.commit().await?;
        return Ok(id); // 重放静默
    }
    if status != AlertStatus::Active {
        return Err(DeviceError::Conflict("当前状态不允许认领".into()));
    }
    alert::ack_update_tx(&mut tx, id, actor.id).await?;
    let after = json!({"id": id, "status": "acknowledged"});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "alert_event",
        "ack",
        "alert_event",
        &id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// 根据心跳证据自动恢复告警：逐条核对活跃告警——
/// 找不到来源事件就不判（缺证据不恢复）；心跳不晚于最新故障也不判（防乱序重放）；
/// 「该指标是否已恢复」由调用方注入的检查器决定（故障码翻译留在网关，设备域不绑协议）。
pub async fn resolve_recovered_for_heartbeat(
    pool: &MySqlPool,
    device_id: &str,
    heartbeat_at: chrono::DateTime<chrono::Utc>,
    recovered_check: &(dyn Fn(&str) -> bool + Send + Sync),
) -> StationResult<()> {
    for (alert_id, created_month, metric, event_key) in
        alert::list_active_for_recovery(pool, device_id).await?
    {
        // 最新故障时刻取告警来源事件的接收时间；查不到时不判定。
        let Some(fault_at) = alert::event_received_at(pool, &event_key).await? else {
            continue;
        };
        if heartbeat_at <= fault_at {
            continue; // 心跳不晚于最新故障：不能作为恢复证据（水印语义）。
        }
        if recovered_check(&metric) {
            alert::auto_resolve(pool, alert_id, created_month, &event_key, heartbeat_at).await?;
        }
    }
    Ok(())
}
