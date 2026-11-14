//! 告警：设备故障事件 → 告警生成/合并/恢复判定（心跳水位防乱序重放）+ 后台列表/认领。
//! 同步入口（record_fault / resolve_recovered）由阶段 06 的设备事件链调用；
//! 本轮交付服务函数与后台查询路由。

use super::{DeviceError, StationResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use chrono::Datelike as _;
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// 记录一条设备故障事件为告警（阶段 06 调用）：
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
    // 站点告警幂等投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type AlertIdemRow = (u64, String, String, Option<String>, Option<String>);
    let existing: Option<AlertIdemRow> = sqlx::query_as(
        "SELECT `id`, `event_id`, `status`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
                DATE_FORMAT(`resolved_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') \
         FROM `alert_event` WHERE `device_id` = ? AND `metric` = ? AND `rule_id` IS NULL \
         ORDER BY `created_at` DESC, `id` DESC LIMIT 1 FOR UPDATE",
    )
    .bind(device_id)
    .bind(metric)
    .fetch_optional(&mut *tx)
    .await?;
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
        if status == "active" || status == "acknowledged" {
            // 升级合并：保留首次 created_at
            sqlx::query("UPDATE `alert_event` SET `event_id` = ?, `value` = ? WHERE `id` = ?")
                .bind(event_id)
                .bind(value as f64)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    // 新告警
    let month = match chrono::Utc::now().date_naive().with_day(1) {
        Some(d) => d,
        None => panic!("月首日必然存在"),
    };
    sqlx::query(
        "INSERT INTO `alert_event` (`device_id`, `severity`, `metric`, `value`, `status`, `event_id`, `created_at`, `created_month`) \
         VALUES (?, ?, ?, ?, 'active', ?, ?, ?)",
    )
    .bind(device_id)
    .bind(severity)
    .bind(metric)
    .bind(value as f64)
    .bind(event_id)
    .bind(received_at)
    .bind(month)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 恢复判定（阶段 06 提供心跳数据后调用）：故障消除时置 auto_resolved（带旧 event_id 条件防误清）。
pub async fn auto_resolve(
    pool: &MySqlPool,
    alert_id: u64,
    created_month: chrono::NaiveDate,
    old_event_id: &str,
    resolved_at: chrono::DateTime<chrono::Utc>,
) -> StationResult<bool> {
    let n = sqlx::query(
        "UPDATE `alert_event` SET `status` = 'auto_resolved', `resolved_at` = ? \
         WHERE `id` = ? AND `created_month` = ? AND `event_id` = ? AND `status` IN ('active','acknowledged')",
    )
    .bind(resolved_at)
    .bind(alert_id)
    .bind(created_month)
    .bind(old_event_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// 后台告警列表（倒序，按设备站点+厂商范围——范围本轮全量）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct AlertRow {
    pub id: u64,
    pub created_month: chrono::NaiveDate,
    pub device_id: String,
    pub severity: String,
    pub metric: String,
    pub status: String,
    pub created_at: Option<String>,
}

pub async fn list(pool: &MySqlPool) -> StationResult<Vec<AlertRow>> {
    sqlx::query_as::<_, AlertRow>(
        "SELECT `id`, `created_month`, `device_id`, `severity`, `metric`, `status`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `alert_event` WHERE `status` IN ('active','acknowledged') \
         ORDER BY `created_at` DESC, `id` DESC LIMIT 200",
    )
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
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
    if !actor_locked.permissions.iter().any(|p| p == "alert.ack") {
        return Err(DeviceError::Forbidden);
    }
    let (id, _created_month, status): (u64, chrono::NaiveDate, String) = sqlx::query_as(
        "SELECT `id`, `created_month`, `status` FROM `alert_event` WHERE `id` = ? FOR UPDATE",
    )
    .bind(alert_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DeviceError::NotFound)?;
    if status == "acknowledged" {
        tx.commit().await?;
        return Ok(id); // 重放静默
    }
    if status != "active" {
        return Err(DeviceError::Conflict("当前状态不允许认领".into()));
    }
    sqlx::query("UPDATE `alert_event` SET `status` = 'acknowledged', `acked_by` = ?, `acked_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(actor.id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
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
