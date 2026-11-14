//! 设备故障工单：用户提交/查询 + 后台派单/解决（状态机 open→dispatched→fixed→closed，
//! 同状态重放静默成功，跳级/回退 409；fixed 须非空 note——旧版语义）。

use super::{DeviceError, StationResult};
use chargepilot_domain::device::{
    FaultReportEventType, FaultReportSource, FaultReportStatus, FaultType,
};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde_json::{Value, json};
use sqlx::MySqlPool;

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

/// 用户提交报修：未知设备号也受理（不补建设备，旧版语义）。
pub struct FaultSubmission {
    pub device_id: String,
    pub fault_type: String,
    pub description: Option<String>,
    pub images: Vec<String>,
}

pub async fn submit(
    pool: &MySqlPool,
    user_id: u64,
    input: FaultSubmission,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> StationResult<u64> {
    if input.device_id.is_empty() || input.device_id.chars().count() > 64 {
        return Err(DeviceError::Input("请提供有效的设备编号".into()));
    }
    let fault_type = FaultType::parse(input.fault_type.as_str())
        .map_err(|_| DeviceError::Input("故障类型无效".into()))?;
    if input
        .description
        .as_ref()
        .is_some_and(|d| d.chars().count() > 2000)
    {
        return Err(DeviceError::Input("描述最多 2000 字".into()));
    }
    if input.images.len() > 9 {
        return Err(DeviceError::Input("图片最多 9 张".into()));
    }
    let mut tx = pool.begin().await?;
    let id = allocator.next_id().await.map_err(DeviceError::from)?;
    sqlx::query(
        "INSERT INTO `device_fault_report` (`id`, `device_id`, `user_id`, `report_source`, `fault_type`, `description`, `images_json`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(&input.device_id)
    .bind(user_id)
    .bind(FaultReportSource::User)
    .bind(fault_type)
    .bind(&input.description)
    .bind(if input.images.is_empty() { None } else { Some(json!(input.images)) })
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
    tx.commit().await?;
    let _ = (ip, request_id, Value::Null);
    Ok(id)
}

/// 本人报修列表。
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

/// 本人报修进度（仅 user_visible 事件正序）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct FaultEventRow {
    pub event_type: String,
    pub from_status: String,
    pub to_status: String,
    pub note: Option<String>,
    pub created_at: Option<String>,
}

pub async fn my_history(
    pool: &MySqlPool,
    user_id: u64,
    report_id: u64,
) -> StationResult<Vec<FaultEventRow>> {
    let owned: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM `device_fault_report` WHERE `id` = ? AND `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(report_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    if owned.is_none() {
        return Err(DeviceError::NotFound);
    }
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

// ---------------- 后台 ----------------

/// 后台工单列表（状态筛选 + 分页；范围过滤待数据范围启用，本轮全量）。
pub async fn admin_list(
    pool: &MySqlPool,
    status: Option<&str>,
    page: u32,
    page_size: u32,
) -> StationResult<(Vec<Value>, i64)> {
    let allowed = status.is_none_or(|s| matches!(s, "open" | "dispatched" | "fixed" | "closed"));
    if !allowed {
        return Err(DeviceError::Input("工单状态无效".into()));
    }
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_fault_report` WHERE `deleted_at` IS NULL AND (? IS NULL OR `status` = ?)",
    )
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    let items: Vec<Value> = sqlx::query(
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
    .collect();
    Ok((items, total))
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

/// 相邻状态机（旧版 CanTransitionFault）。
fn can_transition(from: FaultReportStatus, to: FaultReportStatus) -> bool {
    use FaultReportStatus::*;
    matches!(
        (from, to),
        (Open, Dispatched) | (Dispatched, Fixed) | (Fixed, Closed)
    )
}

/// 派单：工单锁内核对 → 仅 open|dispatched 可派 → 同人重放静默 → 写状态+事件+审计。
pub async fn dispatch(
    pool: &MySqlPool,
    actor: &Profile,
    report_id: u64,
    assigned_to: u64,
    note: Option<String>,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    if let Some(n) = &note
        && n.chars().count() > 2000
    {
        return Err(DeviceError::Input("备注最多 2000 字".into()));
    }
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "fault.dispatch")
    {
        return Err(DeviceError::Forbidden);
    }
    let (current, assigned): (FaultReportStatus, Option<u64>) = sqlx::query_as(
        "SELECT `status`, `assigned_to` FROM `device_fault_report` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(report_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DeviceError::NotFound)?;
    if current != FaultReportStatus::Open && current != FaultReportStatus::Dispatched {
        return Err(DeviceError::Conflict("当前状态不允许派单".into()));
    }
    if assigned == Some(assigned_to) {
        tx.commit().await?;
        return Ok(report_id); // 同人重放静默
    }
    // 被指派人须存在、启用、有 fault.resolve 权限
    let assignee_ok: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM `admin_user` u \
         JOIN `role` r ON r.`id` = u.`role_id` AND r.`deleted_at` IS NULL \
         JOIN `role_permission` rp ON rp.`role_id` = r.`id` \
         JOIN `permission` p ON p.`id` = rp.`permission_id` AND p.`code` = 'fault.resolve' \
         WHERE u.`id` = ? AND u.`status` = 'active' AND u.`deleted_at` IS NULL",
    )
    .bind(assigned_to)
    .fetch_optional(&mut *tx)
    .await?;
    if assignee_ok.is_none() {
        return Err(DeviceError::Input("被指派人不存在或无处理权限".into()));
    }
    let event_type = if current == FaultReportStatus::Open {
        FaultReportEventType::Dispatched
    } else {
        FaultReportEventType::Reassigned
    };
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
    .bind(actor.id)
    .bind(event_type)
    .bind(current.as_str())
    .bind(FaultReportStatus::Dispatched.as_str())
    .bind(assigned_to)
    .bind(&note)
    .execute(&mut *tx)
    .await?;
    let after = json!({"report_id": report_id, "assigned_to": assigned_to, "status": "dispatched"});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "device_fault_report",
        "dispatch",
        "device_fault_report",
        &report_id.to_string(),
        &json!({"from": current}),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(report_id)
}

/// 解决/关闭：仅当前处理人可操作；相邻迁移 + fixed 须非空 note。
pub async fn resolve(
    pool: &MySqlPool,
    actor: &Profile,
    report_id: u64,
    to_status: &str,
    note: Option<String>,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    let to_status = FaultReportStatus::parse(to_status)
        .map_err(|_| DeviceError::Input("目标状态无效".into()))?;
    if !matches!(
        to_status,
        FaultReportStatus::Fixed | FaultReportStatus::Closed
    ) {
        return Err(DeviceError::Input("目标状态无效".into()));
    }
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "fault.resolve")
    {
        return Err(DeviceError::Forbidden);
    }
    let (current, assigned): (FaultReportStatus, Option<u64>) = sqlx::query_as(
        "SELECT `status`, `assigned_to` FROM `device_fault_report` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(report_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DeviceError::NotFound)?;
    if assigned != Some(actor.id) {
        return Err(DeviceError::Conflict("仅当前处理人可操作该工单".into()));
    }
    if current == to_status {
        tx.commit().await?;
        return Ok(report_id); // 同状态重放静默
    }
    if !can_transition(current, to_status) {
        return Err(DeviceError::Conflict(format!(
            "工单不允许从 {} 迁移到 {}",
            current.as_str(),
            to_status.as_str()
        )));
    }
    if to_status == FaultReportStatus::Fixed && note.as_ref().is_none_or(|n| n.trim().is_empty()) {
        return Err(DeviceError::Input("标记已修复须填写处理说明".into()));
    }
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
    .bind(actor.id)
    .bind(to_status)
    .bind(current.as_str())
    .bind(to_status.as_str())
    .bind(&note)
    .execute(&mut *tx)
    .await?;
    let after = json!({"report_id": report_id, "status": to_status.as_str()});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "device_fault_report",
        to_status.as_str(),
        "device_fault_report",
        &report_id.to_string(),
        &json!({"from": current.as_str()}),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(report_id)
}
