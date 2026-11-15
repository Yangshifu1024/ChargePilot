//! 站点 CRUD 用例：输入校验 → 事务内创建或锁行更新 → 审计。SQL 在 repo/station.rs。

use crate::repo::station;
use crate::{DeviceError, StationResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct Station {
    pub id: u64,
    pub name: String,
    pub address: Option<String>,
    /// DECIMAL 原样字符串（前端契约：经纬度不做数值化处理）
    pub longitude: String,
    pub latitude: String,
    pub status: String,
    pub contact_phone: Option<String>,
}

#[derive(Deserialize)]
pub struct StationInput {
    /// 已废弃字段：非 null 即拒绝
    #[serde(default)]
    pub code: Option<String>,
    pub name: String,
    #[serde(default)]
    pub address: Option<String>,
    pub longitude: f64,
    pub latitude: f64,
    pub status: String,
    #[serde(default)]
    pub contact_phone: Option<String>,
}

impl StationInput {
    fn validate(&self) -> Result<(), DeviceError> {
        if self.code.is_some() {
            return Err(DeviceError::Input(
                "站点编码已废弃，请勿提交 code 字段".into(),
            ));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 128 {
            return Err(DeviceError::Input("请填写站点名称（最多128字）".into()));
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            return Err(DeviceError::Input("经度须在 [-180, 180] 之间".into()));
        }
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            return Err(DeviceError::Input("纬度须在 [-90, 90] 之间".into()));
        }
        if !matches!(self.status.as_str(), "active" | "disabled" | "construction") {
            return Err(DeviceError::Input("站点状态无效".into()));
        }
        if self.contact_phone.as_ref().is_some_and(|p| p.len() > 32) {
            return Err(DeviceError::Input("联系电话过长".into()));
        }
        Ok(())
    }
}

pub async fn create(
    pool: &MySqlPool,
    actor: &Profile,
    input: StationInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> StationResult<u64> {
    input.validate()?;
    let id = allocator.next_id().await.map_err(DeviceError::from)?;
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::STATION_CREATE)
    {
        return Err(DeviceError::Forbidden);
    }
    station::insert_station_tx(
        &mut tx,
        id,
        input.name.trim(),
        input.address.as_deref(),
        input.longitude,
        input.latitude,
        &input.status,
        input.contact_phone.as_deref(),
    )
    .await?;
    let after = json!({"id": id, "name": input.name, "status": input.status});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "station",
        "create",
        "station",
        &id.to_string(),
        &json!(Value::Null),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(
    pool: &MySqlPool,
    actor: &Profile,
    station_id: u64,
    input: StationInput,
    ip: &str,
    request_id: &str,
) -> StationResult<u64> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::STATION_UPDATE)
    {
        return Err(DeviceError::Forbidden);
    }
    station::lock_station_tx(&mut tx, station_id).await?;
    station::update_station_tx(
        &mut tx,
        station_id,
        input.name.trim(),
        input.address.as_deref(),
        input.longitude,
        input.latitude,
        &input.status,
        input.contact_phone.as_deref(),
    )
    .await?;
    let after = json!({"id": station_id, "name": input.name, "status": input.status});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "station",
        "update",
        "station",
        &station_id.to_string(),
        &json!(Value::Null),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(station_id)
}

pub async fn get(pool: &MySqlPool, station_id: u64) -> StationResult<Station> {
    station::station_by_id(pool, station_id).await
}

pub async fn list(
    pool: &MySqlPool,
    page: chargepilot_mod_ops::management::Page,
    keyword: Option<&str>,
    status: Option<&str>,
) -> StationResult<(Vec<Station>, i64)> {
    if let Some(s) = status
        && !matches!(s, "active" | "disabled" | "construction")
    {
        return Err(DeviceError::Input("站点状态无效".into()));
    }
    let kw = format!(
        "%{}%",
        keyword
            .unwrap_or_default()
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    );
    let total = station::station_count(pool, &kw, status).await?;
    let items = station::station_page(pool, &kw, status, page).await?;
    Ok((items, total))
}
