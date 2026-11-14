//! 设备导入：受理（幂等登记 + 身份核对 + 兼容检查）→ 执行（provision + 台账落库）同事务塌缩。
//! ADR 001 修订（2026-11-14）：旧版「central 受理 → HTTP → gateway 开通」的跨进程两跳
//! 在单体内合并为一个业务事务；device_provision/device/device_port 的导入写入归 device 域。

use super::{DeviceError, StationResult};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

pub const MAX_BATCH: usize = 100;

#[derive(Deserialize, serde::Serialize)]
pub struct ImportInput {
    pub import_id: String,
    pub devices: Vec<ImportDevice>,
}

#[derive(Deserialize, serde::Serialize, Clone)]
pub struct ImportDevice {
    /// 8–32 位 [A-Za-z0-9_-]
    pub device_id: String,
    pub vendor_id: u64,
    pub station_id: u64,
    #[serde(default = "default_port_count")]
    pub port_count: u32,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub charge_mode: Option<String>,
}

fn default_port_count() -> u32 {
    2
}

/// 受理 + 执行（单事务塌缩，ADR 001 修订）：
/// 1. 输入校验（批量 ≤100、device_id 8–32 位、批内忽略大小写判重）
/// 2. 厂商须 enabled 且 dc589+tcp，port_count ≤20
/// 3. 按忽略大小写 device_id 排序锁定（防死锁）；device_import_identity INSERT IGNORE
///    首次登记 + 锁读比对（同号不同配置 → 409）
/// 4. provision 幂等写入（device_provision INSERT IGNORE + 比对）+ device/device_port 台账
/// 5. device_meta 台账创建（status=enabled）+ 导入任务完成 + 审计
pub async fn import_devices(
    pool: &MySqlPool,
    actor: &Profile,
    input: ImportInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> StationResult<u64> {
    if input.import_id.is_empty() || input.import_id.len() > 64 {
        return Err(DeviceError::Input("导入编号无效".into()));
    }
    if input.devices.is_empty() || input.devices.len() > MAX_BATCH {
        return Err(DeviceError::Input(format!("每批须为 1–{MAX_BATCH} 台设备")));
    }
    let mut seen = std::collections::BTreeSet::new();
    for d in &input.devices {
        let id_ok = (8..=32).contains(&d.device_id.len())
            && d.device_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !id_ok {
            return Err(DeviceError::Input(format!(
                "设备编号 {} 无效（须为 8–32 位字母数字）",
                d.device_id
            )));
        }
        if d.vendor_id == 0 || d.station_id == 0 {
            return Err(DeviceError::Input("设备必须归属厂商与站点".into()));
        }
        if d.port_count == 0 || d.port_count > 20 {
            return Err(DeviceError::Input("端口数须为 1–20".into()));
        }
        let lower = d.device_id.to_lowercase();
        if !seen.insert(lower) {
            return Err(DeviceError::Input(format!(
                "设备编号 {} 批内重复",
                d.device_id
            )));
        }
    }
    // 忽略大小写排序（锁定顺序，防死锁）
    let mut devices = input.devices.clone();
    devices.sort_by(|a, b| a.device_id.to_lowercase().cmp(&b.device_id.to_lowercase()));

    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "device.import")
    {
        return Err(DeviceError::Forbidden);
    }

    // 导入任务幂等登记
    let inserted = sqlx::query(
        "INSERT IGNORE INTO `device_import` (`import_id`, `actor_id`, `request_json`, `status`) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&input.import_id)
    .bind(actor.id)
    .bind(serde_json::to_value(&input)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let _ = inserted;
    let (status, request_json): (String, serde_json::Value) = sqlx::query_as(
        "SELECT `status`, `request_json` FROM `device_import` WHERE `import_id` = ? FOR UPDATE",
    )
    .bind(&input.import_id)
    .fetch_one(&mut *tx)
    .await?;
    let parsed_again: ImportInput = serde_json::from_value(request_json)?;
    if parsed_again.devices.len() != input.devices.len() {
        return Err(DeviceError::Conflict("导入请求与首次提交不一致".into()));
    }
    if status == "completed" {
        tx.commit().await?;
        return Ok(0); // 重放：已完成任务不再开通不再审计
    }

    // 逐台开通
    let mut provisioned = 0u64;
    for d in &devices {
        // 厂商核对（enabled + dc589 + tcp）
        let vendor_ok: Option<(String, String, String)> = sqlx::query_as(
            "SELECT `adapter_class`, `protocol`, `status` FROM `vendor` WHERE `id` = ? AND `deleted_at` IS NULL FOR SHARE",
        )
        .bind(d.vendor_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((adapter, protocol, vstatus)) = vendor_ok else {
            return Err(DeviceError::Input(format!("厂商 {} 不存在", d.vendor_id)));
        };
        if vstatus != "enabled" || protocol != "tcp" || adapter != "dc589" {
            return Err(DeviceError::Input(format!(
                "厂商 {} 未启用或协议不支持",
                d.vendor_id
            )));
        }
        // 站点核对（active）
        let station_ok: Option<u64> =
            sqlx::query_scalar("SELECT `id` FROM `station` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR SHARE")
                .bind(d.station_id)
                .fetch_optional(&mut *tx)
                .await?;
        if station_ok.is_none() {
            return Err(DeviceError::Input(format!(
                "设备 {} 的站点不可用",
                d.device_id
            )));
        }
        // 导入身份幂等登记（同号不同配置 → 409）
        let fresh = sqlx::query(
            "INSERT IGNORE INTO `device_import_identity` (`device_id`, `request_json`) VALUES (?, ?)",
        )
        .bind(&d.device_id)
        .bind(serde_json::to_value(&d)?)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let known: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT `request_json` FROM `device_import_identity` WHERE `device_id` = ? FOR UPDATE",
        )
        .bind(&d.device_id)
        .fetch_optional(&mut *tx)
        .await?;
        match (fresh, known) {
            (0, Some(prev)) if prev != serde_json::to_value(&d)? => {
                return Err(DeviceError::Conflict(format!(
                    "设备 {} 与既有导入配置不一致",
                    d.device_id
                )));
            }
            _ => {}
        }
        // provision 幂等（device_provision 首次原文 + 比对）
        let provision_request = json!({"device_id": d.device_id, "vendor_id": d.vendor_id, "station_id": d.station_id, "port_count": d.port_count});
        let _prov_fresh = sqlx::query(
            "INSERT IGNORE INTO `device_provision` (`device_id`, `request_json`) VALUES (?, ?)",
        )
        .bind(&d.device_id)
        .bind(&provision_request)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let prov_known: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT `request_json` FROM `device_provision` WHERE `device_id` = ? FOR UPDATE",
        )
        .bind(&d.device_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(prev) = prov_known {
            if prev != provision_request {
                return Err(DeviceError::Conflict(format!(
                    "设备 {} 与既有开通配置不一致",
                    d.device_id
                )));
            }
        }
        // device 台账（gateway 运行时读）：已存在则核对归属，不存在则创建 + 端口
        let device_row: Option<(u64, u64)> = sqlx::query_as(
            "SELECT `id`, `vendor_id` FROM `device` WHERE `device_id` = ? FOR UPDATE",
        )
        .bind(&d.device_id)
        .fetch_optional(&mut *tx)
        .await?;
        match device_row {
            Some((_, dev_vendor)) => {
                if dev_vendor != d.vendor_id {
                    return Err(DeviceError::Conflict(format!(
                        "设备 {} 归属厂商不一致",
                        d.device_id
                    )));
                }
            }
            None => {
                let dev_pk = allocator.next_id().await.map_err(DeviceError::from)?;
                sqlx::query(
                    "INSERT INTO `device` (`id`, `device_id`, `vendor_id`, `station_id`, `port_count`) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(dev_pk)
                .bind(&d.device_id)
                .bind(d.vendor_id)
                .bind(d.station_id)
                .bind(d.port_count)
                .execute(&mut *tx)
                .await?;
                for port in 1..=d.port_count {
                    sqlx::query(
                        "INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, ?, ?, 'idle')",
                    )
                    // device_port.device_id 是 varchar 设备全局编号（schema 注释
                    // 真相源），与扫描/遥测/占用各读写点一致，不是 device.id 数字键。
                    .bind(&d.device_id)
                    .bind(port)
                    .bind(format!("{}-{}", d.device_id, port))
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        // device_meta 台账（central 侧）：已存在核对归属不覆盖；不存在创建
        let meta: Option<u64> =
            sqlx::query_scalar("SELECT `id` FROM `device_meta` WHERE `device_id` = ? AND `deleted_at` IS NULL FOR UPDATE")
                .bind(&d.device_id)
                .fetch_optional(&mut *tx)
                .await?;
        if meta.is_none() {
            let meta_id = allocator.next_id().await.map_err(DeviceError::from)?;
            sqlx::query(
                "INSERT INTO `device_meta` (`id`, `device_id`, `station_id`, `vendor_id`, `model`, `protocol_adapter`, `charge_mode`, `status`) \
                 VALUES (?, ?, ?, ?, ?, 'dc589', ?, 'enabled')",
            )
            .bind(meta_id)
            .bind(&d.device_id)
            .bind(d.station_id)
            .bind(d.vendor_id)
            .bind(&d.model)
            .bind(d.charge_mode.as_deref().unwrap_or("device_duration"))
            .execute(&mut *tx)
            .await?;
        }
        provisioned += 1;
    }

    // 任务完成 + 审计
    sqlx::query(
        "UPDATE `device_import` SET `status` = 'completed', `attempts` = `attempts` + 1, `retryable` = 0 WHERE `import_id` = ?",
    )
    .bind(&input.import_id)
    .execute(&mut *tx)
    .await?;
    let after = json!({"import_id": input.import_id, "provisioned": provisioned});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "device_import",
        "import",
        "device_import",
        &input.import_id,
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(provisioned)
}

/// 导入任务列表（最近 100 个）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct ImportJobRow {
    pub import_id: String,
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

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
