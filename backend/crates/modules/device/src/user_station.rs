//! C 端站点查询（无需登录）：附近站点（球面余弦距离）与站点详情（设备心跳在线）。

use super::{DeviceError, StationResult};
use serde::Serialize;
use sqlx::MySqlPool;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct NearbyStation {
    pub id: u64,
    pub name: String,
    pub address: Option<String>,
    /// DECIMAL 原样字符串（前端契约）
    pub longitude: String,
    pub latitude: String,
    pub status: String,
    pub contact_phone: Option<String>,
    pub distance_km: Option<String>,
}

#[derive(Serialize)]
pub struct NearbyPage {
    pub items: Vec<NearbyStation>,
    pub page: u32,
    pub page_size: u32,
    pub located: bool,
}

/// 附近站点：坐标可选但须成对；radius 1–50km 默认 50；球面余弦距离 SQL（旧版公式）。
pub async fn nearby(
    pool: &MySqlPool,
    lon: Option<f64>,
    lat: Option<f64>,
    radius_km: f64,
    page: u32,
    page_size: u32,
) -> StationResult<NearbyPage> {
    match (lon, lat) {
        (Some(lo), Some(la)) => {
            if !(-180.0..=180.0).contains(&lo) || !(-90.0..=90.0).contains(&la) {
                return Err(DeviceError::Input("坐标超出范围".into()));
            }
            if !(1.0..=50.0).contains(&radius_km) {
                return Err(DeviceError::Input("搜索半径须为 1–50 公里".into()));
            }
            let items = sqlx::query_as::<_, NearbyStation>(
                "SELECT * FROM (SELECT `id`, `name`, `address`, \
                        CAST(`longitude` AS CHAR) AS `longitude`, CAST(`latitude` AS CHAR) AS `latitude`, \
                        `status`, `contact_phone`, \
                        CAST(ROUND(6371 * ACOS(LEAST(1, \
                          COS(RADIANS(?)) * COS(RADIANS(`latitude`)) * COS(RADIANS(`longitude`) - RADIANS(?)) + \
                          SIN(RADIANS(?)) * SIN(RADIANS(`latitude`)))), 3) AS CHAR) AS `distance_km` \
                 FROM `station` WHERE `status` = 'active' AND `deleted_at` IS NULL) t \
                 WHERE t.`distance_km` <= ? \
                 ORDER BY t.`distance_km` LIMIT ? OFFSET ?",
            )
            .bind(la)
            .bind(lo)
            .bind(la)
            .bind(radius_km)
            .bind(page_size as i64)
            .bind(((page - 1) as i64) * (page_size as i64))
            .fetch_all(pool)
            .await?;
            Ok(NearbyPage {
                items,
                page,
                page_size,
                located: true,
            })
        }
        (None, None) => {
            let items = sqlx::query_as::<_, NearbyStation>(
                "SELECT `id`, `name`, `address`, \
                        CAST(`longitude` AS CHAR) AS `longitude`, CAST(`latitude` AS CHAR) AS `latitude`, \
                        `status`, `contact_phone`, NULL AS `distance_km` \
                 FROM `station` WHERE `status` = 'active' AND `deleted_at` IS NULL \
                 ORDER BY `created_at` DESC, `id` DESC LIMIT ? OFFSET ?",
            )
            .bind(page_size as i64)
            .bind(((page - 1) as i64) * (page_size as i64))
            .fetch_all(pool)
            .await?;
            Ok(NearbyPage {
                items,
                page,
                page_size,
                located: false,
            })
        }
        _ => Err(DeviceError::Input("经纬度必须成对提供".into())),
    }
}

#[derive(Debug, Serialize)]
pub struct StationDetail {
    pub id: u64,
    pub name: String,
    pub address: Option<String>,
    pub longitude: String,
    pub latitude: String,
    pub status: String,
    pub contact_phone: Option<String>,
    /// 设备运行信息（心跳来自 gateway 运行时；阶段 06 前恒为离线）
    pub devices: Vec<DeviceBrief>,
    pub announcements: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DeviceBrief {
    pub device_id: String,
    pub status: String,
    pub last_heartbeat_at: Option<String>,
    pub online: bool,
    pub runtime_available: bool,
}

/// 站点详情：仅 active 未删；设备 = 未删且非 retired 的 device_meta（心跳 1 小时内视为在线）。
pub async fn detail(pool: &MySqlPool, station_id: u64) -> StationResult<StationDetail> {
    let head = sqlx::query_as::<_, NearbyStation>(
        "SELECT `id`, `name`, `address`, \
                CAST(`longitude` AS CHAR) AS `longitude`, CAST(`latitude` AS CHAR) AS `latitude`, \
                `status`, `contact_phone`, NULL AS `distance_km` \
         FROM `station` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(station_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeviceError::NotFound)?;
    let devices = sqlx::query_as::<_, DeviceBrief>(
        "SELECT d.`device_id`, d.`status`, \
                DATE_FORMAT(g.`last_heartbeat_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `last_heartbeat_at`, \
                (g.`last_heartbeat_at` IS NOT NULL AND g.`last_heartbeat_at` >= DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 1 HOUR)) AS `online`, \
                FALSE AS `runtime_available` \
         FROM `device_meta` d LEFT JOIN `device` g ON g.`device_id` = d.`device_id` \
         WHERE d.`station_id` = ? AND d.`deleted_at` IS NULL AND d.`status` <> 'retired' \
         ORDER BY d.`device_id`",
    )
    .bind(station_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|mut d| {
        if d.last_heartbeat_at.is_none() {
            d.online = false;
        }
        d
    })
    .collect();
    Ok(StationDetail {
        id: head.id,
        name: head.name,
        address: head.address,
        longitude: head.longitude,
        latitude: head.latitude,
        status: head.status,
        contact_phone: head.contact_phone,
        devices,
        announcements: Vec::new(),
    })
}
