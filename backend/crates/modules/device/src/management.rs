//! 设备台账管理（device_meta）：列表/详情/资料更新（乐观锁）/运营启停。
//! 语义对齐旧版：资料更新全字段必填 + expected_updated_at 乐观锁；
//! 状态可写子集只有 enabled|disabled（fault/retired 不可经此接口设置）。
//! 运行态合并（站点/厂商名、心跳/信号/端口、runtime_available）对齐旧版
//! Management.Enrich：设备行存在即可读（单体同库直查等价旧版摘要成功）。

use super::{DeviceError, StationResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

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
    /// 对外标签数组（tags_json 列直出，NULL 读为空数组；响应名 tags 对齐旧版）。
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

/// 资料更新输入（全字段必填：serde 区分缺失与 null，旧版 RawMessage 语义）。
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
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_meta` d LEFT JOIN `station` s ON s.`id` = d.`station_id` \
         WHERE d.`deleted_at` IS NULL \
         AND (? IS NULL OR d.`station_id` = ?) AND (? IS NULL OR d.`status` = ?) \
         AND (d.`device_id` LIKE ? ESCAPE '!' OR d.`model` LIKE ? ESCAPE '!' OR s.`name` LIKE ? ESCAPE '!')",
    )
    .bind(station_id).bind(station_id).bind(status).bind(status)
    .bind(&kw).bind(&kw).bind(&kw)
    .fetch_one(pool)
    .await?;
    // 运行态直查同库事实（旧版经网关摘要合并，单体等价）：心跳/信号/信号采样
    // 来自 device 表心跳 UPSERT；runtime_available = 网关已登记该设备。
    let mut items = sqlx::query_as::<_, DeviceRow>(
        "SELECT d.`id`, d.`device_id`, d.`station_id`, s.`name` AS `station_name`, \
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
         LEFT JOIN `device` dev ON dev.`device_id` = d.`device_id` \
         WHERE d.`deleted_at` IS NULL \
         AND (? IS NULL OR d.`station_id` = ?) AND (? IS NULL OR d.`status` = ?) \
         AND (d.`device_id` LIKE ? ESCAPE '!' OR d.`model` LIKE ? ESCAPE '!' OR s.`name` LIKE ? ESCAPE '!') \
         ORDER BY d.`id` DESC LIMIT ? OFFSET ?",
    )
    .bind(station_id).bind(station_id).bind(status).bind(status)
    .bind(&kw).bind(&kw).bind(&kw)
    .bind(page.page_size as i64)
    .bind(((page.page - 1) as i64) * (page.page_size as i64))
    .fetch_all(pool)
    .await?;
    attach_ports(pool, &mut items).await?;
    Ok((items, total))
}

pub async fn get(pool: &MySqlPool, id: u64) -> StationResult<DeviceRow> {
    let mut rows = sqlx::query_as::<_, DeviceRow>(
        "SELECT d.`id`, d.`device_id`, d.`station_id`, s.`name` AS `station_name`, \
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
         LEFT JOIN `device` dev ON dev.`device_id` = d.`device_id` \
         WHERE d.`id` = ? AND d.`deleted_at` IS NULL LIMIT 1",
    )
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
    let current: Option<(String,)> = sqlx::query_as(
        "SELECT DATE_FORMAT(`updated_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') FROM `device_meta` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((updated_at,)) = current else {
        return Err(DeviceError::NotFound);
    };
    if updated_at != input.expected_updated_at {
        return Err(DeviceError::Conflict(
            "设备资料已被修改，请刷新后重试".into(),
        ));
    }
    sqlx::query(
        "UPDATE `device_meta` SET \
           `model` = ?, `serial_no` = ?, `warranty_until` = ?, `tags_json` = ?, \
           `updated_at` = GREATEST(`updated_at`, UTC_TIMESTAMP(3), DATE_ADD(`updated_at`, INTERVAL 1 MICROSECOND)) \
         WHERE `id` = ?",
    )
    .bind(&input.model)
    .bind(&input.serial_no)
    .bind(warranty)
    .bind(tags)
    .bind(id)
    .execute(&mut *tx)
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

/// 运营启停：仅 enabled|disabled 可写；不停止已有订单（旧版语义）。
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
    let current: Option<String> = sqlx::query_scalar(
        "SELECT `status` FROM `device_meta` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Err(DeviceError::NotFound);
    };
    if current == "fault" || current == "retired" {
        return Err(DeviceError::Conflict(
            "故障或退役设备不能直接启停，请先处理设备状态".into(),
        ));
    }
    sqlx::query(
        "UPDATE `device_meta` SET `status` = ?, `updated_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(status)
    .bind(id)
    .execute(&mut *tx)
    .await?;
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
