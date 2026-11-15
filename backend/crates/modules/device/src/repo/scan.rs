//! 扫码解析表访问（蓝图第五章 repo 层）：端口/设备联表查询与运营状态读取。

use chargepilot_domain::device::{DeviceStatus, VendorStatus};
use sqlx::MySqlPool;

/// 端口与设备联表行。
pub(crate) type PortRow = (
    String,                                // port_code
    String,                                // device_id
    u8,                                    // port_no
    String,                                // status
    Option<String>,                        // current_order_id
    Option<chrono::DateTime<chrono::Utc>>, // last_seen_at
    Option<u64>,                           // station_id
);

const SCAN_FROM: &str = "FROM `device_port` AS `p` \
     JOIN `device` AS `d` ON `d`.`device_id` = `p`.`device_id` \
     JOIN `vendor` AS `v` ON `v`.`id` = `d`.`vendor_id` \
     WHERE `p`.`deleted_at` IS NULL AND `d`.`status` = ? AND `d`.`deleted_at` IS NULL \
       AND `v`.`status` = ? AND `v`.`deleted_at` IS NULL";

/// 共享片段的两个状态条件绑定（占位符序在其它条件之前）。
const SCAN_STATUS_BINDS: (DeviceStatus, VendorStatus) =
    (DeviceStatus::Enabled, VendorStatus::Enabled);

const SCAN_COLS: &str = "SELECT `p`.`port_code`, `p`.`device_id`, `p`.`port_no`, \
         CAST(`p`.`status` AS CHAR), `p`.`current_order_id`, `d`.`last_seen_at`, `d`.`station_id`";

/// 读取中央运营状态（缺失按读取错误处理）。
pub async fn device_meta_status(
    pool: &MySqlPool,
    device_id: &str,
) -> Result<String, crate::ScanError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `device_meta` \
         WHERE `device_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    status
        .ok_or_else(|| crate::ScanError::Storage(anyhow::anyhow!("device_meta 缺行: {device_id}")))
}

/// 按端口二维码编码查单端口（enabled 设备与厂商）。
pub async fn port_by_code(
    pool: &MySqlPool,
    code: &str,
) -> Result<Option<PortRow>, crate::ScanError> {
    let sql = format!("{SCAN_COLS} {SCAN_FROM} AND `p`.`port_code` = ? LIMIT 1");
    sqlx::query_as::<_, PortRow>(&sql)
        .bind(SCAN_STATUS_BINDS.0)
        .bind(SCAN_STATUS_BINDS.1)
        .bind(code)
        .fetch_optional(pool)
        .await
        .map_err(crate::ScanError::from)
}

/// 按设备业务编号查端口列表（端口号升序）。
pub async fn ports_by_device(
    pool: &MySqlPool,
    device_id: &str,
) -> Result<Vec<PortRow>, crate::ScanError> {
    let sql = format!("{SCAN_COLS} {SCAN_FROM} AND `d`.`device_id` = ? ORDER BY `p`.`port_no`");
    sqlx::query_as::<_, PortRow>(&sql)
        .bind(SCAN_STATUS_BINDS.0)
        .bind(SCAN_STATUS_BINDS.1)
        .bind(device_id)
        .fetch_all(pool)
        .await
        .map_err(crate::ScanError::from)
}
