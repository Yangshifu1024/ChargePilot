//! 设备故障工单用例：用户提交/查询 + 后台派单/解决（状态机 open→dispatched→fixed→closed，
//! 同状态重放静默成功，跳级/回退 409；fixed 须非空 note）。SQL 在 repo/fault.rs。

use crate::repo::fault;
use crate::{DeviceError, StationResult};
use chargepilot_domain::device::{FaultReportEventType, FaultReportStatus, FaultType};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde_json::{Value, json};
use sqlx::MySqlPool;

pub use crate::repo::fault::{FaultEventRow, FaultReportRow};

/// 用户提交报修：未知设备号也受理（不补建设备）。
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
    fault::insert_user_report_tx(
        &mut tx,
        id,
        user_id,
        &input.device_id,
        fault_type,
        input.description.as_ref(),
        if input.images.is_empty() {
            None
        } else {
            Some(json!(input.images))
        },
    )
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
    fault::list_mine(pool, user_id, page, page_size).await
}

pub async fn my_history(
    pool: &MySqlPool,
    user_id: u64,
    report_id: u64,
) -> StationResult<Vec<FaultEventRow>> {
    if !fault::owns_report(pool, user_id, report_id).await? {
        return Err(DeviceError::NotFound);
    }
    fault::user_history(pool, report_id).await
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
    let total = fault::admin_count(pool, status).await?;
    let items = fault::admin_rows(pool, status, page, page_size).await?;
    Ok((items, total))
}

/// 后台流转历史（全事件倒序）。
pub async fn admin_history(pool: &MySqlPool, report_id: u64) -> StationResult<Vec<FaultEventRow>> {
    fault::admin_history(pool, report_id).await
}

/// 相邻状态机。
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
    let (current, assigned) = fault::lock_report_tx(&mut tx, report_id)
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
    if !fault::assignee_qualified_tx(&mut tx, assigned_to).await? {
        return Err(DeviceError::Input("被指派人不存在或无处理权限".into()));
    }
    let event_type = if current == FaultReportStatus::Open {
        FaultReportEventType::Dispatched
    } else {
        FaultReportEventType::Reassigned
    };
    fault::dispatch_report_tx(
        &mut tx,
        report_id,
        actor.id,
        event_type,
        current,
        assigned_to,
        note.as_ref(),
    )
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
    let (current, assigned) = fault::lock_report_tx(&mut tx, report_id)
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
    fault::resolve_report_tx(
        &mut tx,
        report_id,
        actor.id,
        current,
        to_status,
        note.as_ref(),
    )
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
