//! vendor 表访问（蓝图第五章 repo 层）。

use crate::service::vendor::Vendor;
use crate::{DeviceError, VendorResult};
use chargepilot_domain::device::{VendorProtocol, VendorStatus};
use sqlx::{MySqlConnection, MySqlPool};

/// 厂商列表（only_enabled 过滤，id 正序）。
pub async fn vendor_list(pool: &MySqlPool, only_enabled: bool) -> VendorResult<Vec<Vendor>> {
    let (filter, only): (&str, Option<VendorStatus>) = if only_enabled {
        ("AND `status` = ?", Some(VendorStatus::Enabled))
    } else {
        ("", None)
    };
    let sql = format!(
        "SELECT `id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status` \
         FROM `vendor` WHERE `deleted_at` IS NULL {filter} ORDER BY `id`"
    );
    let mut q = sqlx::query_as::<_, Vendor>(&sql);
    if let Some(status) = only {
        q = q.bind(status);
    }
    q.fetch_all(pool).await.map_err(DeviceError::from)
}

/// 单厂商行。
pub async fn vendor_by_id(pool: &MySqlPool, id: u64) -> VendorResult<Vendor> {
    sqlx::query_as::<_, Vendor>(
        "SELECT `id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status` \
         FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeviceError::NotFound)
}

/// 创建厂商；编码唯一键冲突原样上抛（调用方按业务冲突分类）。
pub async fn insert_vendor_tx(
    tx: &mut MySqlConnection,
    id: u64,
    vendor_code: &str,
    vendor_name: &str,
    adapter_class: &str,
    protocol: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `protocol`, `status`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(vendor_code)
    .bind(vendor_name)
    .bind(adapter_class)
    .bind(protocol)
    .bind(status)
    .execute(tx)
    .await?;
    Ok(())
}

/// 锁厂商行读不变约束（id, vendor_code, adapter_class, protocol）。
pub async fn lock_vendor_tx(
    tx: &mut MySqlConnection,
    id: u64,
) -> VendorResult<Option<(u64, String, String, String)>> {
    sqlx::query_as::<_, (u64, String, String, String)>(
        "SELECT `id`, `vendor_code`, `adapter_class`, `protocol` FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 厂商名下未删除设备计数（协议变更约束核对）。
pub async fn device_meta_count(
    pool_conn: &mut MySqlConnection,
    vendor_id: u64,
) -> VendorResult<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_meta` WHERE `vendor_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(vendor_id)
    .fetch_one(pool_conn)
    .await
    .map_err(DeviceError::from)
}

/// 更新厂商；首次启用写 enabled_at。
#[allow(clippy::too_many_arguments)]
pub async fn update_vendor_tx(
    tx: &mut MySqlConnection,
    id: u64,
    vendor_name: &str,
    adapter_class: &str,
    protocol: VendorProtocol,
    status: VendorStatus,
) -> VendorResult<()> {
    sqlx::query(
        "UPDATE `vendor` SET `vendor_name` = ?, `adapter_class` = ?, `protocol` = ?, `status` = ?, \
         `enabled_at` = IF(? = ? AND `enabled_at` IS NULL, UTC_TIMESTAMP(3), `enabled_at`) WHERE `id` = ?",
    )
    .bind(vendor_name)
    .bind(adapter_class)
    .bind(protocol)
    .bind(status)
    .bind(status)
    .bind(VendorStatus::Enabled)
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}
