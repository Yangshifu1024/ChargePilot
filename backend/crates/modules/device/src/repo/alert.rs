//! alert_event 表访问（蓝图第五章 repo 层）。

use crate::{DeviceError, StationResult};
use chargepilot_domain::device::AlertStatus;
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{MySqlConnection, MySqlPool};

/// 告警行（后台列表投影）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct AlertRow {
    pub id: u64,
    pub created_month: NaiveDate,
    pub device_id: String,
    pub severity: String,
    pub metric: String,
    pub status: AlertStatus,
    pub created_at: Option<String>,
}

/// 同维度最新告警幂等投影（FOR UPDATE）：
/// (id, event_id, status, created_at 文本, resolved_at 文本)。
pub async fn latest_device_metric_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    metric: &str,
) -> StationResult<Option<(u64, String, AlertStatus, Option<String>, Option<String>)>> {
    sqlx::query_as(
        "SELECT `id`, `event_id`, `status`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
                DATE_FORMAT(`resolved_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') \
         FROM `alert_event` WHERE `device_id` = ? AND `metric` = ? AND `rule_id` IS NULL \
         ORDER BY `created_at` DESC, `id` DESC LIMIT 1 FOR UPDATE",
    )
    .bind(device_id)
    .bind(metric)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 升级合并：仅更新 event_id 与 value（保留首次 created_at）。
pub async fn upgrade_alert_tx(
    tx: &mut MySqlConnection,
    id: u64,
    event_id: &str,
    value: i64,
) -> StationResult<()> {
    sqlx::query("UPDATE `alert_event` SET `event_id` = ?, `value` = ? WHERE `id` = ?")
        .bind(event_id)
        .bind(value as f64)
        .bind(id)
        .execute(tx)
        .await?;
    Ok(())
}

/// 插入新 active 告警（月分区列 created_month）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_alert_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    severity: &str,
    metric: &str,
    value: i64,
    event_id: &str,
    received_at: DateTime<Utc>,
    created_month: NaiveDate,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `alert_event` (`device_id`, `severity`, `metric`, `value`, `status`, `event_id`, `created_at`, `created_month`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(device_id)
    .bind(severity)
    .bind(metric)
    .bind(value as f64)
    .bind(AlertStatus::Active)
    .bind(event_id)
    .bind(received_at)
    .bind(created_month)
    .execute(tx)
    .await?;
    Ok(())
}

/// 恢复判定落库：故障消除时置 auto_resolved（带旧 event_id 条件防误清）。
pub async fn auto_resolve(
    pool: &MySqlPool,
    alert_id: u64,
    created_month: NaiveDate,
    old_event_id: &str,
    resolved_at: DateTime<Utc>,
) -> StationResult<bool> {
    let n = sqlx::query(
        "UPDATE `alert_event` SET `status` = ?, `resolved_at` = ? \
         WHERE `id` = ? AND `created_month` = ? AND `event_id` = ? AND `status` IN (?, ?)",
    )
    .bind(AlertStatus::AutoResolved)
    .bind(resolved_at)
    .bind(alert_id)
    .bind(created_month)
    .bind(old_event_id)
    .bind(AlertStatus::Active)
    .bind(AlertStatus::Acknowledged)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// 按设备查活跃/已确认告警的恢复判定字段（心跳恢复编排用；
/// rule_id IS NULL 排除人工规则告警——自动恢复只针对故障事实告警）。
pub async fn list_active_for_recovery(
    pool: &MySqlPool,
    device_id: &str,
) -> StationResult<Vec<(u64, NaiveDate, String, String)>> {
    sqlx::query_as(
        "SELECT `id`, `created_month`, `metric`, `event_id` FROM `alert_event` \
         WHERE `device_id` = ? AND `rule_id` IS NULL AND `status` IN (?, ?)",
    )
    .bind(device_id)
    .bind(AlertStatus::Active)
    .bind(AlertStatus::Acknowledged)
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}

/// 按事件键查来源告警的接收时刻（恢复证据）。device_event 是网关接入域
/// 的表——告警行引用其事件键，补全证据属告警生命周期语义（跨域读一列）。
pub async fn event_received_at(
    pool: &MySqlPool,
    event_key: &str,
) -> StationResult<Option<DateTime<Utc>>> {
    sqlx::query_scalar("SELECT `received_at` FROM `device_event` WHERE `event_key` = ?")
        .bind(event_key)
        .fetch_optional(pool)
        .await
        .map_err(DeviceError::from)
}

/// 后台告警列表（active/acknowledged，倒序，上限 200）。
pub async fn list_active_rows(pool: &MySqlPool) -> StationResult<Vec<AlertRow>> {
    sqlx::query_as::<_, AlertRow>(
        "SELECT `id`, `created_month`, `device_id`, `severity`, `metric`, `status`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `alert_event` WHERE `status` IN (?, ?) \
         ORDER BY `created_at` DESC, `id` DESC LIMIT 200",
    )
    .bind(AlertStatus::Active)
    .bind(AlertStatus::Acknowledged)
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}

/// 锁告警行读状态（FOR UPDATE）。
pub async fn lock_alert_tx(
    tx: &mut MySqlConnection,
    alert_id: u64,
) -> StationResult<Option<(u64, NaiveDate, AlertStatus)>> {
    sqlx::query_as(
        "SELECT `id`, `created_month`, `status` FROM `alert_event` WHERE `id` = ? FOR UPDATE",
    )
    .bind(alert_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 认领落库：acknowledged + 认领人与时刻。
pub async fn ack_update_tx(tx: &mut MySqlConnection, id: u64, actor_id: u64) -> StationResult<()> {
    sqlx::query("UPDATE `alert_event` SET `status` = ?, `acked_by` = ?, `acked_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(AlertStatus::Acknowledged)
        .bind(actor_id)
        .bind(id)
        .execute(tx)
        .await?;
    Ok(())
}
