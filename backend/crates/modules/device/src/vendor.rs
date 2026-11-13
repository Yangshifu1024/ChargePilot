//! 厂商管理：单体内直接读写 vendor 表（旧版经 central→gateway HTTP 代理，
//! 单体后代理塌缩为进程内调用；约束照搬：编码不可变、有设备时协议不可改、仅支持 dc589+TCP）。

use super::{DeviceError, VendorResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::json;
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
        if !matches!(self.protocol.as_str(), "tcp" | "mqtt" | "hybrid") {
            return Err(DeviceError::Input("接入协议无效".into()));
        }
        if !matches!(self.status.as_str(), "enabled" | "disabled") {
            return Err(DeviceError::Input("厂商状态无效".into()));
        }
        Ok(())
    }
}

pub async fn list(pool: &MySqlPool, only_enabled: bool) -> VendorResult<Vec<Vendor>> {
    let filter = if only_enabled {
        "AND `status` = 'enabled'"
    } else {
        ""
    };
    let sql = format!(
        "SELECT `id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status` \
         FROM `vendor` WHERE `deleted_at` IS NULL {filter} ORDER BY `id`"
    );
    sqlx::query_as::<_, Vendor>(&sql)
        .fetch_all(pool)
        .await
        .map_err(DeviceError::from)
}

pub async fn get(pool: &MySqlPool, id: u64) -> VendorResult<Vendor> {
    sqlx::query_as::<_, Vendor>(
        "SELECT `id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status` \
         FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeviceError::NotFound)
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
        .any(|p| p == "vendor.create")
    {
        return Err(DeviceError::Forbidden);
    }
    let id = allocator.next_id().await.map_err(DeviceError::from)?;
    sqlx::query(
        "INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(input.vendor_code.trim())
    .bind(input.vendor_name.trim())
    .bind(&input.adapter_class)
    .bind(&input.protocol)
    .bind(&input.status)
    .execute(&mut *tx)
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
        .any(|p| p == "vendor.update")
    {
        return Err(DeviceError::Forbidden);
    }
    let existing = sqlx::query_as::<_, (u64, String, String, String)>(
        "SELECT `id`, `vendor_code`, `adapter_class`, `protocol` FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(DeviceError::NotFound)?;
    // 编码不可变；已有设备时 adapter/protocol 不可改（旧版语义）
    if input.adapter_class != existing.2 || input.protocol != existing.3 {
        let has_devices: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM `device_meta` WHERE `vendor_id` = ? AND `deleted_at` IS NULL",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
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
    // 首次启用写 enabled_at
    sqlx::query(
        "UPDATE `vendor` SET `vendor_name` = ?, `adapter_class` = ?, `protocol` = ?, `status` = ?, \
         `enabled_at` = IF(? = 'enabled' AND `enabled_at` IS NULL, UTC_TIMESTAMP(3), `enabled_at`) WHERE `id` = ?",
    )
    .bind(input.vendor_name.trim())
    .bind(&input.adapter_class)
    .bind(&input.protocol)
    .bind(&input.status)
    .bind(&input.status)
    .bind(id)
    .execute(&mut *tx)
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
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return DeviceError::Conflict(msg.into());
        }
    }
    DeviceError::Storage(e.into())
}

use serde_json::Value;
