//! 设备台账管理用例：列表/详情/资料更新（乐观锁）/运营启停。
//! 语义：资料更新全字段必填 + expected_updated_at 乐观锁；
//! 状态可写子集只有 enabled|disabled（fault/retired 不可经此接口设置）。
//! 运行态合并（站点/厂商名、心跳/信号/端口、runtime_available）：
//! 设备行存在即可读（单体同库直查）。SQL 在 repo/management.rs。

use crate::repo::management;
use crate::{DeviceError, StationResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

pub use crate::repo::management::{DevicePortRow, DeviceRow};

/// 资料更新输入（全字段必填：serde 区分缺失与 null）。
#[derive(Deserialize)]
pub struct DeviceInput {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(default)]
    pub warranty_until: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub expected_updated_at: String,
}

fn validate_tags(tags: &[String]) -> Result<(), DeviceError> {
    if tags.len() > 20 {
        return Err(DeviceError::Input("标签最多 20 项".into()));
    }
    let uniq: std::collections::BTreeSet<_> = tags.iter().collect();
    if uniq.len() != tags.len() {
        return Err(DeviceError::Input("标签不能重复".into()));
    }
    if tags.iter().any(|t| t.is_empty() || t.chars().count() > 32) {
        return Err(DeviceError::Input("标签须为 1–32 字".into()));
    }
    Ok(())
}

pub async fn list(
    pool: &MySqlPool,
    page: chargepilot_mod_ops::management::Page,
    station_id: Option<u64>,
    status: Option<&str>,
    keyword: Option<&str>,
) -> StationResult<(Vec<DeviceRow>, i64)> {
    if let Some(s) = status
        && !matches!(s, "enabled" | "disabled" | "retired" | "fault")
    {
        return Err(DeviceError::Input("设备状态无效".into()));
    }
    let kw = format!(
        "%{}%",
        keyword
            .unwrap_or_default()
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    );
    let total = management::device_count(pool, station_id, status, &kw).await?;
    let items = management::device_page(pool, station_id, status, &kw, page).await?;
    Ok((items, total))
}

pub async fn get(pool: &MySqlPool, id: u64) -> StationResult<DeviceRow> {
    management::device_by_id(pool, id).await
}

/// 资料更新：锁设备 → expected_updated_at 乐观锁 → 只写资料字段 → 审计（同事务）。
pub async fn update(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
    input: DeviceInput,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    let tags = match &input.tags {
        Some(t) => {
            validate_tags(t)?;
            Some(json!(t))
        }
        None => None,
    };
    let warranty = match &input.warranty_until {
        Some(w) if !w.is_empty() => Some(
            chrono::DateTime::parse_from_rfc3339(w)
                .map_err(|_| DeviceError::Input("保修截止时间格式无效".into()))?
                .with_timezone(&chrono::Utc),
        ),
        _ => None,
    };
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "device.operate")
    {
        return Err(DeviceError::Forbidden);
    }
    let updated_at = management::lock_updated_at_tx(&mut tx, id)
        .await?
        .ok_or(DeviceError::NotFound)?;
    if updated_at != input.expected_updated_at {
        return Err(DeviceError::Conflict(
            "设备资料已被修改，请刷新后重试".into(),
        ));
    }
    management::update_device_tx(
        &mut tx,
        id,
        input.model.as_deref(),
        input.serial_no.as_deref(),
        warranty,
        tags,
    )
    .await?;
    let after = json!({"id": id, "model": input.model, "serial_no": input.serial_no});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "device_meta",
        "update",
        "device_meta",
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

/// 运营启停：仅 enabled|disabled 可写；不停止已有订单。
pub async fn set_status(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
    status: &str,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    if !matches!(status, "enabled" | "disabled") {
        return Err(DeviceError::Input(
            "设备状态无效：仅可切换启用或停用".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "device.operate")
    {
        return Err(DeviceError::Forbidden);
    }
    let current = management::lock_status_tx(&mut tx, id)
        .await?
        .ok_or(DeviceError::NotFound)?;
    if current == "fault" || current == "retired" {
        return Err(DeviceError::Conflict(
            "故障或退役设备不能直接启停，请先处理设备状态".into(),
        ));
    }
    management::set_device_status_tx(&mut tx, id, status).await?;
    let after = json!({"id": id, "status": status});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "device_meta",
        "update_status",
        "device_meta",
        &id.to_string(),
        &json!({"from": current}),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
