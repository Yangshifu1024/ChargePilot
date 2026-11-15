//! device_import / _identity / _provision / device / device_port / device_meta
//! 导入事务表访问（蓝图第五章 repo 层）。

use crate::{DeviceError, StationResult};
use chargepilot_domain::device::{DeviceImportStatus, DeviceMetaStatus, DevicePortStatus};
use serde_json::Value;
use sqlx::{MySqlConnection, MySqlPool};

/// 导入任务行（最近 100 列表投影）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct ImportJobRow {
    pub import_id: String,
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// 幂等登记导入任务（INSERT IGNORE；是否首插由 rows_affected 判断）。
pub async fn register_import_tx(
    tx: &mut MySqlConnection,
    import_id: &str,
    actor_id: u64,
    request_json: &Value,
) -> StationResult<u64> {
    Ok(sqlx::query(
        "INSERT IGNORE INTO `device_import` (`import_id`, `actor_id`, `request_json`, `status`) VALUES (?, ?, ?, ?)",
    )
    .bind(import_id)
    .bind(actor_id)
    .bind(request_json)
    .bind(DeviceImportStatus::Pending)
    .execute(tx)
    .await?
    .rows_affected())
}

/// 锁任务行读状态与首次请求原文（FOR UPDATE）。
pub async fn lock_import_tx(
    tx: &mut MySqlConnection,
    import_id: &str,
) -> StationResult<(DeviceImportStatus, Value)> {
    sqlx::query_as(
        "SELECT `status`, `request_json` FROM `device_import` WHERE `import_id` = ? FOR UPDATE",
    )
    .bind(import_id)
    .fetch_one(tx)
    .await
    .map_err(DeviceError::from)
}

/// 任务完成（attempts+1，retryable 清零）。
pub async fn complete_import_tx(tx: &mut MySqlConnection, import_id: &str) -> StationResult<()> {
    sqlx::query(
        "UPDATE `device_import` SET `status` = ?, `attempts` = `attempts` + 1, `retryable` = 0 WHERE `import_id` = ?",
    )
    .bind(DeviceImportStatus::Completed)
    .bind(import_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// SHARE 锁读厂商 (adapter_class, protocol, status)。
pub async fn vendor_share_tx(
    tx: &mut MySqlConnection,
    vendor_id: u64,
) -> StationResult<Option<(String, String, String)>> {
    sqlx::query_as(
        "SELECT `adapter_class`, `protocol`, `status` FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(vendor_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// SHARE 锁读启用站点主键。
pub async fn active_station_share_tx(
    tx: &mut MySqlConnection,
    station_id: u64,
) -> StationResult<Option<u64>> {
    sqlx::query_scalar(
        "SELECT `id` FROM `station` WHERE `id` = ? AND `status` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(station_id)
    .bind(chargepilot_domain::device::StationStatus::Active)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 导入身份幂等登记（INSERT IGNORE）。
pub async fn register_identity_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    request_json: &Value,
) -> StationResult<u64> {
    Ok(sqlx::query(
        "INSERT IGNORE INTO `device_import_identity` (`device_id`, `request_json`) VALUES (?, ?)",
    )
    .bind(device_id)
    .bind(request_json)
    .execute(tx)
    .await?
    .rows_affected())
}

/// 锁读既有导入身份配置（FOR UPDATE）。
pub async fn identity_request_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
) -> StationResult<Option<Value>> {
    sqlx::query_scalar(
        "SELECT `request_json` FROM `device_import_identity` WHERE `device_id` = ? FOR UPDATE",
    )
    .bind(device_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// provision 幂等登记（INSERT IGNORE 原文）。
pub async fn register_provision_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    request_json: &Value,
) -> StationResult<u64> {
    Ok(sqlx::query(
        "INSERT IGNORE INTO `device_provision` (`device_id`, `request_json`) VALUES (?, ?)",
    )
    .bind(device_id)
    .bind(request_json)
    .execute(tx)
    .await?
    .rows_affected())
}

/// 锁读既有开通配置（FOR UPDATE）。
pub async fn provision_request_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
) -> StationResult<Option<Value>> {
    sqlx::query_scalar(
        "SELECT `request_json` FROM `device_provision` WHERE `device_id` = ? FOR UPDATE",
    )
    .bind(device_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 锁读 gateway 设备行 (id, vendor_id)（FOR UPDATE）。
pub async fn device_row_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
) -> StationResult<Option<(u64, u64)>> {
    sqlx::query_as("SELECT `id`, `vendor_id` FROM `device` WHERE `device_id` = ? FOR UPDATE")
        .bind(device_id)
        .fetch_optional(tx)
        .await
        .map_err(DeviceError::from)
}

/// 创建 gateway 设备行。
pub async fn insert_device_tx(
    tx: &mut MySqlConnection,
    dev_pk: u64,
    device_id: &str,
    vendor_id: u64,
    station_id: u64,
    port_count: u32,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `device` (`id`, `device_id`, `vendor_id`, `station_id`, `port_count`) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(dev_pk)
    .bind(device_id)
    .bind(vendor_id)
    .bind(station_id)
    .bind(port_count)
    .execute(tx)
    .await?;
    Ok(())
}

/// 创建端口行（port_code = 设备号-端口号）。
pub async fn insert_device_port_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    port_no: u32,
    port_code: String,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, ?, ?, ?)",
    )
    // device_port.device_id 是 varchar 设备全局编号（schema 注释
    // 真相源），与扫描/遥测/占用各读写点一致，不是 device.id 数字键。
    .bind(device_id)
    .bind(port_no)
    .bind(port_code)
    .bind(DevicePortStatus::Idle)
    .execute(tx)
    .await?;
    Ok(())
}

/// 锁读台账主键（FOR UPDATE）。
pub async fn meta_id_tx(tx: &mut MySqlConnection, device_id: &str) -> StationResult<Option<u64>> {
    sqlx::query_scalar(
        "SELECT `id` FROM `device_meta` WHERE `device_id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(device_id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 创建 central 侧台账（enabled）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_device_meta_tx(
    tx: &mut MySqlConnection,
    meta_id: u64,
    device_id: &str,
    station_id: u64,
    vendor_id: u64,
    model: Option<&str>,
    charge_mode: &str,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `device_meta` (`id`, `device_id`, `station_id`, `vendor_id`, `model`, `protocol_adapter`, `charge_mode`, `status`) \
         VALUES (?, ?, ?, ?, ?, 'dc589', ?, ?)",
    )
    .bind(meta_id)
    .bind(device_id)
    .bind(station_id)
    .bind(vendor_id)
    .bind(model)
    .bind(charge_mode)
    .bind(DeviceMetaStatus::Enabled)
    .execute(tx)
    .await?;
    Ok(())
}

/// 导入任务列表（最近 100 个）。
pub async fn list_imports(pool: &MySqlPool) -> StationResult<Vec<ImportJobRow>> {
    sqlx::query_as::<_, ImportJobRow>(
        "SELECT `import_id`, `status`, `attempts`, `last_error`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                DATE_FORMAT(`updated_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `updated_at` \
         FROM `device_import` ORDER BY `created_at` DESC LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}
