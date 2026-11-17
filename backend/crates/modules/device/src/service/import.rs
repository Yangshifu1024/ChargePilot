//! 设备导入用例：受理（幂等登记 + 身份核对 + 兼容检查）→ 执行（provision + 台账落库）同事务塌缩。
//! 导入写入（device_provision/device/device_port/device_meta）归 device 域，
//! SQL 原语在 repo/import.rs。

use crate::repo::import;
use crate::{DeviceError, StationResult};
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

/// 受理 + 执行（单事务塌缩）：
/// 1. 输入校验（批量 ≤100、device_id 8–32 位、批内忽略大小写判重）
/// 2. 厂商须 enabled 且 dc589+tcp，port_count ≤20
/// 3. 按忽略大小写 device_id 排序锁定（防死锁）；导入身份 INSERT IGNORE
/// 首次登记 + 锁读比对（同号不同配置 → 409）
/// 4. provision 幂等写入（INSERT IGNORE + 比对）+ device/device_port 台账
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
    devices.sort_by_key(d_sort_key);

    let mut tx = pool.begin().await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(DeviceError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == chargepilot_mod_ops::perms::DEVICE_IMPORT)
    {
        return Err(DeviceError::Forbidden);
    }

    // 导入任务幂等登记
    import::register_import_tx(
        &mut tx,
        &input.import_id,
        actor.id,
        &serde_json::to_value(&input)?,
    )
    .await?;
    let (status, request_json) = import::lock_import_tx(&mut tx, &input.import_id).await?;
    let parsed_again: ImportInput = serde_json::from_value(request_json)?;
    if parsed_again.devices.len() != input.devices.len() {
        return Err(DeviceError::Conflict("导入请求与首次提交不一致".into()));
    }
    if status == chargepilot_domain::device::DeviceImportStatus::Completed {
        tx.commit().await?;
        return Ok(0); // 重放：已完成任务不再开通不再审计
    }

    // 逐台开通
    let mut provisioned = 0u64;
    for d in &devices {
        // 厂商核对（enabled + dc589 + tcp）
        let Some((adapter, protocol, vstatus)) =
            import::vendor_share_tx(&mut tx, d.vendor_id).await?
        else {
            return Err(DeviceError::Input(format!("厂商 {} 不存在", d.vendor_id)));
        };
        if vstatus != "enabled" || protocol != "tcp" || adapter != "dc589" {
            return Err(DeviceError::Input(format!(
                "厂商 {} 未启用或协议不支持",
                d.vendor_id
            )));
        }
        // 站点核对（active）
        if import::active_station_share_tx(&mut tx, d.station_id)
            .await?
            .is_none()
        {
            return Err(DeviceError::Input(format!(
                "设备 {} 的站点不可用",
                d.device_id
            )));
        }
        // 导入身份幂等登记（同号不同配置 → 409）
        let fresh =
            import::register_identity_tx(&mut tx, &d.device_id, &serde_json::to_value(d)?).await?;
        let known = import::identity_request_tx(&mut tx, &d.device_id).await?;
        match (fresh, known) {
            (0, Some(prev)) if prev != serde_json::to_value(d)? => {
                return Err(DeviceError::Conflict(format!(
                    "设备 {} 与既有导入配置不一致",
                    d.device_id
                )));
            }
            _ => {}
        }
        // provision 幂等（首次原文 + 比对）
        let provision_request = json!({"device_id": d.device_id, "vendor_id": d.vendor_id, "station_id": d.station_id, "port_count": d.port_count});
        import::register_provision_tx(&mut tx, &d.device_id, &provision_request).await?;
        let prov_known = import::provision_request_tx(&mut tx, &d.device_id).await?;
        if let Some(prev) = prov_known
            && prev != provision_request
        {
            return Err(DeviceError::Conflict(format!(
                "设备 {} 与既有开通配置不一致",
                d.device_id
            )));
        }
        // device 台账（gateway 运行时读）：已存在则核对归属，不存在则创建 + 端口
        match import::device_row_tx(&mut tx, &d.device_id).await? {
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
                import::insert_device_tx(
                    &mut tx,
                    dev_pk,
                    &d.device_id,
                    d.vendor_id,
                    d.station_id,
                    d.port_count,
                )
                .await?;
                for port in 1..=d.port_count {
                    import::insert_device_port_tx(
                        &mut tx,
                        &d.device_id,
                        port,
                        format!("{}-{}", d.device_id, port),
                    )
                    .await?;
                }
            }
        }
        // device_meta 台账（central 侧）：已存在核对归属不覆盖；不存在创建
        if import::meta_id_tx(&mut tx, &d.device_id).await?.is_none() {
            let meta_id = allocator.next_id().await.map_err(DeviceError::from)?;
            import::insert_device_meta_tx(
                &mut tx,
                meta_id,
                &d.device_id,
                d.station_id,
                d.vendor_id,
                d.model.as_deref(),
                d.charge_mode.as_deref().unwrap_or("device_duration"),
            )
            .await?;
        }
        provisioned += 1;
    }

    // 任务完成 + 审计
    import::complete_import_tx(&mut tx, &input.import_id).await?;
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

/// 排序键：device_id 忽略大小写（锁定顺序防死锁）。
fn d_sort_key(d: &ImportDevice) -> String {
    d.device_id.to_lowercase()
}

/// 导入任务列表（最近 100 个）。
pub use crate::repo::import::ImportJobRow;

pub async fn list_imports(pool: &MySqlPool) -> StationResult<Vec<ImportJobRow>> {
    import::list_imports(pool).await
}
