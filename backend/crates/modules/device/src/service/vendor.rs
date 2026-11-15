//! 厂商管理用例：直接读写 vendor 表（约束：编码不可变、有设备时协议不可改、
//! 仅支持 dc589+TCP）。SQL 在 repo/vendor.rs。

use crate::repo::vendor;
use crate::{DeviceError, VendorResult};
use chargepilot_domain::device::{VendorProtocol, VendorStatus};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct Vendor {
    pub id: u64,
    pub vendor_code: String,
    pub vendor_name: String,
    pub adapter_class: String,
    pub protocol: String,
    pub status: String,
}

#[derive(Deserialize)]
pub struct VendorInput {
    pub vendor_code: String,
    pub vendor_name: String,
    pub adapter_class: String,
    pub protocol: String,
    pub status: String,
}

impl VendorInput {
    fn validate(&self, for_create: bool) -> Result<(), DeviceError> {
        if for_create && (self.vendor_code.trim().is_empty() || self.vendor_code.len() > 64) {
            return Err(DeviceError::Input("请填写厂商编码（最多64字）".into()));
        }
        if self.vendor_name.trim().is_empty() || self.vendor_name.len() > 128 {
            return Err(DeviceError::Input("请填写厂商名称（最多128字）".into()));
        }
        if VendorProtocol::parse(self.protocol.as_str()).is_err() {
            return Err(DeviceError::Input("接入协议无效".into()));
        }
        if VendorStatus::parse(self.status.as_str()).is_err() {
            return Err(DeviceError::Input("厂商状态无效".into()));
        }
        Ok(())
    }
}

pub async fn list(pool: &MySqlPool, only_enabled: bool) -> VendorResult<Vec<Vendor>> {
    vendor::vendor_list(pool, only_enabled).await
}

pub async fn get(pool: &MySqlPool, id: u64) -> VendorResult<Vendor> {
    vendor::vendor_by_id(pool, id).await
}

pub async fn create(
    pool: &MySqlPool,
    actor: &Profile,
    input: VendorInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> VendorResult<u64> {
    input.validate(true)?;
    if input.adapter_class != "dc589" || input.protocol != "tcp" {
        return Err(DeviceError::Input(
            "当前仅支持 dc589 协议与 TCP 直连接入".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::VENDOR_CREATE)
    {
        return Err(DeviceError::Forbidden);
    }
    let id = allocator.next_id().await.map_err(DeviceError::from)?;
    vendor::insert_vendor_tx(
        &mut tx,
        id,
        input.vendor_code.trim(),
        input.vendor_name.trim(),
        &input.adapter_class,
        &input.protocol,
        &input.status,
    )
    .await
    .map_err(|e| unique_conflict(e, "厂商编码已存在"))?;
    let after = json!({"id": id, "vendor_code": input.vendor_code, "status": input.status});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "vendor",
        "create",
        "vendor",
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
    id: u64,
    input: VendorInput,
    ip: &str,
    request_id: &str,
) -> VendorResult<u64> {
    input.validate(false)?;
    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::VENDOR_UPDATE)
    {
        return Err(DeviceError::Forbidden);
    }
    let existing = vendor::lock_vendor_tx(&mut tx, id)
        .await?
        .ok_or(DeviceError::NotFound)?;
    // 编码不可变；已有设备时 adapter/protocol 不可改
    if input.adapter_class != existing.2 || input.protocol != existing.3 {
        let has_devices = vendor::device_meta_count(&mut tx, id).await?;
        if has_devices > 0 {
            return Err(DeviceError::Conflict(
                "厂商已接入设备，不能变更协议或适配器".into(),
            ));
        }
        if input.adapter_class != "dc589" || input.protocol != "tcp" {
            return Err(DeviceError::Input(
                "当前仅支持 dc589 协议与 TCP 直连接入".into(),
            ));
        }
    }
    vendor::update_vendor_tx(
        &mut tx,
        id,
        input.vendor_name.trim(),
        &input.adapter_class,
        VendorProtocol::parse(input.protocol.as_str())
            .map_err(|_| DeviceError::Input("协议无效".into()))?,
        VendorStatus::parse(input.status.as_str())
            .map_err(|_| DeviceError::Input("状态无效".into()))?,
    )
    .await?;
    let after = json!({"id": id, "vendor_name": input.vendor_name, "status": input.status});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "vendor",
        "update",
        "vendor",
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

fn unique_conflict(e: sqlx::Error, msg: &str) -> DeviceError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        return DeviceError::Conflict(msg.into());
    }
    DeviceError::Storage(e.into())
}
