//! device_meta 台账表访问与运行态读模型（蓝图第五章 repo 层）。

use crate::{DeviceError, StationResult};
use sqlx::MySqlPool;

/// 台账运行态行（列表/详情共用投影：站点/厂商名 + 心跳/信号 + runtime_available）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct DeviceRow {
    pub id: u64,
    pub device_id: String,
    pub station_id: Option<u64>,
    pub station_name: Option<String>,
    pub vendor_id: Option<u64>,
    pub vendor_name: Option<String>,
    pub model: Option<String>,
    pub protocol_adapter: String,
    pub charge_mode: String,
    pub serial_no: Option<String>,
    pub status: String,
    pub warranty_until: Option<String>,
    /// 对外标签数组（tags_json 列直出，NULL 读为空数组；响应名 tags）。
    #[serde(rename = "tags")]
    pub tags_json: serde_json::Value,
    /// 资料编辑的并发校验值（expected_updated_at 的来源），UTC 毫秒精度。
    pub updated_at: String,
    pub last_heartbeat_at: Option<String>,
    /// 最近心跳上报的信号强度原始值（tinyint unsigned；0 仍是有效读数）。
    pub signal_strength: Option<u8>,
    pub signal_at: Option<String>,
    #[sqlx(skip)]
    pub ports: Vec<DevicePortRow>,
    pub runtime_available: bool,
}

/// 端口运行态（上报事实：device_port 表；无上报行时数组为空）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct DevicePortRow {
    pub port_no: u64,
    pub status_code: Option<u8>,
    pub status_at: Option<String>,
}

/// 运行态投影 SQL（列表与详情共用 SELECT/FROM）。
const RUNTIME_SELECT: &str = "SELECT d.`id`, d.`device_id`, d.`station_id`, s.`name` AS `station_name`, \
        d.`vendor_id`, v.`vendor_name` AS `vendor_name`, d.`model`, d.`protocol_adapter`, \
        d.`charge_mode`, d.`serial_no`, d.`status`, \
        DATE_FORMAT(d.`warranty_until`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `warranty_until`, \
        COALESCE(d.`tags_json`, '[]') AS `tags_json`, \
        DATE_FORMAT(d.`updated_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `updated_at`, \
        DATE_FORMAT(dev.`last_heartbeat_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `last_heartbeat_at`, \
        dev.`signal_strength`, \
        DATE_FORMAT(dev.`signal_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `signal_at`, \
        (dev.`device_id` IS NOT NULL) AS `runtime_available` \
 FROM `device_meta` d \
 LEFT JOIN `station` s ON s.`id` = d.`station_id` \
 LEFT JOIN `vendor` v ON v.`id` = d.`vendor_id` AND v.`deleted_at` IS NULL \
 LEFT JOIN `device` dev ON dev.`device_id` = d.`device_id`";

/// 台账总数（站点/状态/关键词过滤）。
pub async fn device_count(
    pool: &MySqlPool,
    station_id: Option<u64>,
    status: Option<&str>,
    kw: &str,
) -> StationResult<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_meta` d LEFT JOIN `station` s ON s.`id` = d.`station_id` \
         WHERE d.`deleted_at` IS NULL \
         AND (? IS NULL OR d.`station_id` = ?) AND (? IS NULL OR d.`status` = ?) \
         AND (d.`device_id` LIKE ? ESCAPE '!' OR d.`model` LIKE ? ESCAPE '!' OR s.`name` LIKE ? ESCAPE '!')",
    )
    .bind(station_id).bind(station_id).bind(status).bind(status)
    .bind(kw).bind(kw).bind(kw)
    .fetch_one(pool)
    .await
    .map_err(DeviceError::from)
}

/// 台账运行态分页行（id 倒序）。
#[allow(clippy::too_many_arguments)]
pub async fn device_page(
    pool: &MySqlPool,
    station_id: Option<u64>,
    status: Option<&str>,
    kw: &str,
    page: chargepilot_mod_ops::management::Page,
) -> StationResult<Vec<DeviceRow>> {
    let mut items = sqlx::query_as::<_, DeviceRow>(&format!(
        "{RUNTIME_SELECT} \
         WHERE d.`deleted_at` IS NULL \
         AND (? IS NULL OR d.`station_id` = ?) AND (? IS NULL OR d.`status` = ?) \
         AND (d.`device_id` LIKE ? ESCAPE '!' OR d.`model` LIKE ? ESCAPE '!' OR s.`name` LIKE ? ESCAPE '!') \
         ORDER BY d.`id` DESC LIMIT ? OFFSET ?"
    ))
    .bind(station_id).bind(station_id).bind(status).bind(status)
    .bind(kw).bind(kw).bind(kw)
    .bind(page.page_size as i64)
    .bind(((page.page - 1) as i64) * (page.page_size as i64))
    .fetch_all(pool)
    .await?;
    attach_ports(pool, &mut items).await?;
    Ok(items)
}

/// 单台运行态（含端口补齐）；不存在返回 NotFound。
pub async fn device_by_id(pool: &MySqlPool, id: u64) -> StationResult<DeviceRow> {
    let mut rows = sqlx::query_as::<_, DeviceRow>(&format!(
        "{RUNTIME_SELECT} WHERE d.`id` = ? AND d.`deleted_at` IS NULL LIMIT 1"
    ))
    .bind(id)
    .fetch_all(pool)
    .await?;
    let Some(mut row) = rows.pop() else {
        return Err(DeviceError::NotFound);
    };
    attach_ports(pool, std::slice::from_mut(&mut row)).await?;
    Ok(row)
}

/// 批量补端口运行态（device_port 上报事实；无上报行时保持空数组）。
async fn attach_ports(pool: &MySqlPool, rows: &mut [DeviceRow]) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT `device_id`, `port_no`, `reported_status`, \
         DATE_FORMAT(`reported_status_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `status_at` \
         FROM `device_port` WHERE `device_id` IN (",
    );
    let mut sep = qb.separated(", ");
    for row in rows.iter() {
        sep.push_bind(&row.device_id);
    }
    sep.push_unseparated(") ORDER BY `device_id`, `port_no`");
    let ports: Vec<(String, u64, Option<u8>, Option<String>)> =
        qb.build_query_as().fetch_all(pool).await?;
    let mut by_device: std::collections::HashMap<String, Vec<DevicePortRow>> =
        std::collections::HashMap::new();
    for (device_id, port_no, status_code, status_at) in ports {
        by_device.entry(device_id).or_default().push(DevicePortRow {
            port_no,
            status_code,
            status_at,
        });
    }
    for row in rows.iter_mut() {
        if let Some(list) = by_device.remove(row.device_id.as_str()) {
            row.ports = list;
        }
    }
    Ok(())
}

/// 锁台账行读 updated_at 文本（FOR UPDATE，乐观锁基准）。
pub async fn lock_updated_at_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
) -> StationResult<Option<String>> {
    let current: Option<(String,)> = sqlx::query_as(
        "SELECT DATE_FORMAT(`updated_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') FROM `device_meta` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(tx)
    .await?;
    Ok(current.map(|(s,)| s))
}

/// 资料更新（乐观锁由调用方核对后调用；updated_at 单调推进）。
pub async fn update_device_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
    model: Option<&str>,
    serial_no: Option<&str>,
    warranty: Option<chrono::DateTime<chrono::Utc>>,
    tags: Option<serde_json::Value>,
) -> StationResult<()> {
    sqlx::query(
        "UPDATE `device_meta` SET \
           `model` = ?, `serial_no` = ?, `warranty_until` = ?, `tags_json` = ?, \
           `updated_at` = GREATEST(`updated_at`, UTC_TIMESTAMP(3), DATE_ADD(`updated_at`, INTERVAL 1 MICROSECOND)) \
         WHERE `id` = ?",
    )
    .bind(model)
    .bind(serial_no)
    .bind(warranty)
    .bind(tags)
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}

/// 锁台账行读运营状态（FOR UPDATE）。
pub async fn lock_status_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
) -> StationResult<Option<String>> {
    sqlx::query_scalar(
        "SELECT `status` FROM `device_meta` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(tx)
    .await
    .map_err(DeviceError::from)
}

/// 运营启停落库（updated_at 推进）。
pub async fn set_device_status_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
    status: &str,
) -> StationResult<()> {
    sqlx::query(
        "UPDATE `device_meta` SET `status` = ?, `updated_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(status)
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}
