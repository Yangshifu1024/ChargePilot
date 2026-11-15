//! station 表访问（蓝图第五章 repo 层）。

use crate::service::station::Station;
use crate::{DeviceError, StationResult};
use sqlx::{MySqlConnection, MySqlPool};

/// 创建站点。
#[allow(clippy::too_many_arguments)]
pub async fn insert_station_tx(
    tx: &mut MySqlConnection,
    id: u64,
    name: &str,
    address: Option<&str>,
    longitude: f64,
    latitude: f64,
    status: &str,
    contact_phone: Option<&str>,
) -> StationResult<()> {
    sqlx::query(
        "INSERT INTO `station` (`id`, `name`, `address`, `longitude`, `latitude`, `status`, `contact_phone`) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(name)
    .bind(address)
    .bind(longitude)
    .bind(latitude)
    .bind(status)
    .bind(contact_phone)
    .execute(tx)
    .await?;
    Ok(())
}

/// 锁站点行（FOR UPDATE；已删除视为不存在）。
pub async fn lock_station_tx(tx: &mut MySqlConnection, station_id: u64) -> StationResult<()> {
    sqlx::query("SELECT `id` FROM `station` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE")
        .bind(station_id)
        .fetch_optional(tx)
        .await?
        .ok_or(DeviceError::NotFound)?;
    Ok(())
}

/// 更新站点。
#[allow(clippy::too_many_arguments)]
pub async fn update_station_tx(
    tx: &mut MySqlConnection,
    station_id: u64,
    name: &str,
    address: Option<&str>,
    longitude: f64,
    latitude: f64,
    status: &str,
    contact_phone: Option<&str>,
) -> StationResult<()> {
    sqlx::query(
        "UPDATE `station` SET `name` = ?, `address` = ?, `longitude` = ?, `latitude` = ?, `status` = ?, `contact_phone` = ? WHERE `id` = ?",
    )
    .bind(name)
    .bind(address)
    .bind(longitude)
    .bind(latitude)
    .bind(status)
    .bind(contact_phone)
    .bind(station_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// 单站点行。
pub async fn station_by_id(pool: &MySqlPool, station_id: u64) -> StationResult<Station> {
    sqlx::query_as::<_, Station>(
        "SELECT `id`, `name`, `address`, CAST(`longitude` AS CHAR) AS `longitude`, CAST(`latitude` AS CHAR) AS `latitude`, `status`, `contact_phone` \
         FROM `station` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(station_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeviceError::NotFound)
}

/// 站点总数（关键词与状态过滤；LIKE 退义由调用方完成后传入）。
pub async fn station_count(pool: &MySqlPool, kw: &str, status: Option<&str>) -> StationResult<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `station` WHERE `deleted_at` IS NULL \
         AND (`name` LIKE ? ESCAPE '!' OR `address` LIKE ? ESCAPE '!') AND (? IS NULL OR `status` = ?)",
    )
    .bind(kw)
    .bind(kw)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await
    .map_err(DeviceError::from)
}

/// 站点分页行（与计数同过滤条件，id 倒序）。
pub async fn station_page(
    pool: &MySqlPool,
    kw: &str,
    status: Option<&str>,
    page: chargepilot_mod_ops::management::Page,
) -> StationResult<Vec<Station>> {
    sqlx::query_as::<_, Station>(
        "SELECT `id`, `name`, `address`, CAST(`longitude` AS CHAR) AS `longitude`, CAST(`latitude` AS CHAR) AS `latitude`, `status`, `contact_phone` \
         FROM `station` WHERE `deleted_at` IS NULL \
         AND (`name` LIKE ? ESCAPE '!' OR `address` LIKE ? ESCAPE '!') AND (? IS NULL OR `status` = ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(kw)
    .bind(kw)
    .bind(status)
    .bind(status)
    .bind(page.page_size as i64)
    .bind(((page.page - 1) as i64) * (page.page_size as i64))
    .fetch_all(pool)
    .await
    .map_err(DeviceError::from)
}
