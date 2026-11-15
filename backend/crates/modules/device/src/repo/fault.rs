//! device_fault_report / _event 表访问（蓝图第五章 repo 层）。

use crate::{DeviceError, StationResult};
use chargepilot_domain::device::{
    FaultReportEventType, FaultReportSource, FaultReportStatus, FaultType,
};
use serde_json::{Value, json};
use sqlx::{MySqlConnection, MySqlPool};

/// 工单行（列表投影）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct FaultReportRow {
    pub id: u64,
    pub device_id: String,
    pub fault_type: String,
    pub description: Option<String>,
    pub status: String,
    pub assigned_to: Option<u64>,
    pub created_at: Option<String>,
}

/// 流转事件行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct FaultEventRow {
    pub event_type: String,
    pub from_status: String,
    pub to_status: String,
    pub note: Option<String>,
    pub created_at: Option<String>,
}

/// 用户提交报修落表（open 状态 + Reported 事件）。
pub async fn insert_user_report_tx(
    tx: &mut MySqlConnection,
    id: u64,
    user_id: u64,
    device_id: &str,
    fault_type: FaultType,
    description: Option<&String>,
    images: Option<Value>,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `device_fault_report` (`id`, `device_id`, `user_id`, `report_source`, `fault_type`, `description`, `images_json`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(device_id)
    .bind(user_id)
    .bind(FaultReportSource::User)
    .bind(fault_type)
    .bind(description)
    .bind(images)
    .bind(FaultReportStatus::Open)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO `device_fault_report_event` (`report_id`, `actor_id`, `event_type`, `from_status`, `to_status`, `assigned_to`, `note`, `user_visible`) \
         VALUES (?, ?, ?, ?, ?, ?, '用户提交报修', 1)",
    )
    .bind(id)
    .bind(user_id)
    .bind(FaultReportEventType::Reported)
    .bind("")
    .bind(FaultReportStatus::Open.as_str())
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 本人报修列表（id 倒序）。
pub async fn list_mine(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> StationResult<Vec<FaultReportRow>> {
    sqlx::query_as::<_, FaultReportRow>(
        "SELECT `id`, `device_id`, `fault_type`, `description`, `status`, `assigned_to`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `device_fault_report` WHERE `user_id` = ? AND `deleted_at` IS NULL \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(user_id)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}

/// 归属核对（本人未删除工单）。
pub async fn owns_report(pool: &MySqlPool, user_id: u64, report_id: u64) -> StationResult<bool> {
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM `device_fault_report` WHERE `id` = ? AND `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(report_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(owned.is_some())
}

/// 本人可见流转历史（user_visible 正序）。
pub async fn user_history(pool: &MySqlPool, report_id: u64) -> StationResult<Vec<FaultEventRow>> {
    sqlx::query_as::<_, FaultEventRow>(
        "SELECT `event_type`, `from_status`, `to_status`, `note`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `device_fault_report_event` WHERE `report_id` = ? AND `user_visible` = 1 ORDER BY `id`",
    )
    .bind(report_id)
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}

/// 后台工单计数（状态过滤，None 不过滤）。
pub async fn admin_count(pool: &MySqlPool, status: Option<&str>) -> StationResult<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_fault_report` WHERE `deleted_at` IS NULL AND (? IS NULL OR `status` = ?)",
    )
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await
    .map_err(DeviceError::from)
}

/// 后台工单行（原样列值，json 组装由调用方完成）。
pub async fn admin_rows(
    pool: &MySqlPool,
    status: Option<&str>,
    page: u32,
    page_size: u32,
) -> StationResult<Vec<Value>> {
    Ok(sqlx::query(
        "SELECT `id`, `device_id`, `user_id`, `report_source`, `fault_type`, `description`, \
                `images_json` AS `images`, `status`, `assigned_to`, `resolved_at`, `created_at` \
         FROM `device_fault_report` WHERE `deleted_at` IS NULL AND (? IS NULL OR `status` = ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(status)
    .bind(status)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row: sqlx::mysql::MySqlRow| {
        let mut m = serde_json::Map::new();
        use sqlx::{Column as _, Row as _};
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name().to_string();
            let key = if name == "images_json" {
                "images".to_string()
            } else {
                name
            };
            if let Ok(v) = row.try_get::<Option<serde_json::Value>, _>(i) {
                m.insert(key, v.unwrap_or(Value::Null));
            } else if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                m.insert(key, v.map(|x| json!(x)).unwrap_or(Value::Null));
            } else if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                m.insert(key, v.map(|x| json!(x)).unwrap_or(Value::Null));
            } else if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
                m.insert(key, v.map(|x| json!(x)).unwrap_or(Value::Null));
            }
        }
        Value::Object(m)
    })
    .collect())
}

/// 后台流转历史（全事件倒序）。
pub async fn admin_history(pool: &MySqlPool, report_id: u64) -> StationResult<Vec<FaultEventRow>> {
    sqlx::query_as::<_, FaultEventRow>(
        "SELECT `event_type`, `from_status`, `to_status`, `note`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `device_fault_report_event` WHERE `report_id` = ? ORDER BY `id` DESC",
    )
    .bind(report_id)
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}

/// 锁工单读状态与指派人（FOR UPDATE）。
pub async fn lock_report_tx(
    tx: &mut MySqlConnection,
    report_id: u64,
) -> StationResult<Option<(FaultReportStatus, Option<u64>)>> {
    sqlx::query_as(
        "SELECT `status`, `assigned_to` FROM `device_fault_report` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(report_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 被指派人资格核对：存在、启用、角色含 fault.resolve 权限。
pub async fn assignee_qualified_tx(
    tx: &mut MySqlConnection,
    assigned_to: u64,
) -> StationResult<bool> {
    let ok: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM `admin_user` u \
         JOIN `role` r ON r.`id` = u.`role_id` AND r.`deleted_at` IS NULL \
         JOIN `role_permission` rp ON rp.`role_id` = r.`id` \
         JOIN `permission` p ON p.`id` = rp.`permission_id` AND p.`code` = 'fault.resolve' \
         WHERE u.`id` = ? AND u.`status` = ? AND u.`deleted_at` IS NULL",
    )
    .bind(assigned_to)
    .bind(chargepilot_domain::operations::AdminUserStatus::Active)
    .fetch_optional(tx)
    .await?;
    Ok(ok.is_some())
}

/// 派单落表：状态 → dispatched + 指派人，写 Dispatched/Reassigned 事件。
pub async fn dispatch_report_tx(
    tx: &mut MySqlConnection,
    report_id: u64,
    actor_id: u64,
    event_type: FaultReportEventType,
    from_status: FaultReportStatus,
    assigned_to: u64,
    note: Option<&String>,
) -> StationResult<()> {
    sqlx::query("UPDATE `device_fault_report` SET `status` = ?, `assigned_to` = ? WHERE `id` = ?")
        .bind(FaultReportStatus::Dispatched)
        .bind(assigned_to)
        .bind(report_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO `device_fault_report_event` (`report_id`, `actor_id`, `event_type`, `from_status`, `to_status`, `assigned_to`, `note`, `user_visible`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(report_id)
    .bind(actor_id)
    .bind(event_type)
    .bind(from_status.as_str())
    .bind(FaultReportStatus::Dispatched.as_str())
    .bind(assigned_to)
    .bind(note)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 解决/关闭落表：状态推进（fixed 写 resolved_at）+ 事件。
pub async fn resolve_report_tx(
    tx: &mut MySqlConnection,
    report_id: u64,
    actor_id: u64,
    from_status: FaultReportStatus,
    to_status: FaultReportStatus,
    note: Option<&String>,
) -> StationResult<()> {
    sqlx::query("UPDATE `device_fault_report` SET `status` = ?, `resolved_at` = IF(? = ?, UTC_TIMESTAMP(3), `resolved_at`) WHERE `id` = ?")
        .bind(to_status)
        .bind(to_status)
        .bind(FaultReportStatus::Fixed)
        .bind(report_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO `device_fault_report_event` (`report_id`, `actor_id`, `event_type`, `from_status`, `to_status`, `note`, `user_visible`) \
         VALUES (?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(report_id)
    .bind(actor_id)
    .bind(to_status)
    .bind(from_status.as_str())
    .bind(to_status.as_str())
    .bind(note)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
