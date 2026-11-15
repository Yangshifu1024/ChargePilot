//! 设备执行能力查询（旧版 internal/device/operation_repo.go 的 Capabilities +
//! internal/domain/charging/capabilities.go 的 ProtocolCapabilities）：
//! 按站点和设备号读取唯一未删除设备的已冻结协议标签，映射协议支持的
//! 计费模式、时长上限与上报能力。不能用型号、遥测或客户端声明代替协议。

use crate::DeviceError;
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// 设备号规则：8–32 位字母/数字/下划线/连字符（旧版 deviceIDPattern）。
pub fn valid_device_id(device_id: &str) -> bool {
    !device_id.is_empty()
        && (8..=32).contains(&device_id.len())
        && device_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// DC589 协议的固定能力（旧版 ProtocolCapabilities 的 dc589 分支）。
/// 新增协议时在此登记：能力由协议决定，不按型号或客户端声明推测。
fn protocol_capabilities(adapter: &str) -> Option<Value> {
    match adapter {
        "dc589" => Some(json!({
            "reports_energy": true,
            "reports_segmented_power": true,
            "stop_when_full": true,
            "max_minutes": 4320,
            "duration": true,
            "energy": true,
            "online_card": true,
        })),
        _ => None,
    }
}

/// 查询设备协议与执行能力，返回响应体（旧版 ExecutionCapabilities 形状：
/// protocol_adapter + capabilities + 顶层冗余 reports_energy/reports_segmented_power）。
/// 协议未知时报输入错误；设备缺失时 404。
pub async fn execution_capabilities(
    pool: &MySqlPool,
    station_id: u64,
    device_id: &str,
) -> Result<Value, DeviceError> {
    if !valid_device_id(device_id) {
        return Err(DeviceError::Input("请选择设备".into()));
    }
    let adapter: Option<String> = sqlx::query_scalar(
        "SELECT `protocol_adapter` FROM `device_meta` \
         WHERE `station_id` = ? AND `device_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(station_id)
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .filter(|a: &String| !a.is_empty());
    let Some(adapter) = adapter else {
        return Err(DeviceError::NotFound);
    };
    let capabilities = protocol_capabilities(&adapter)
        .ok_or_else(|| DeviceError::Input(format!("不支持的设备协议：{adapter}")))?;
    let reports_energy = capabilities["reports_energy"].as_bool().unwrap_or(false);
    let reports_segmented_power = capabilities["reports_segmented_power"]
        .as_bool()
        .unwrap_or(false);
    Ok(json!({
        "protocol_adapter": adapter,
        "capabilities": capabilities,
        "reports_energy": reports_energy,
        "reports_segmented_power": reports_segmented_power,
    }))
}
