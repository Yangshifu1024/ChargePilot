//! 匿名扫码解析（旧版 gateway scan_repo + device DiscoveryService 组合对齐）：
//! 先按端口编码查询，再按设备号读取端口列表；中央运营状态补充可用性。
//! 本查询不创建支付或会话；码值校验（裸码字符集与 HTTPS 链接提取）由 HTTP 层完成。

use serde::Serialize;
use sqlx::MySqlPool;

/// 扫码域错误（HTTP 层映射：NotFound → 404 1004，Storage → 503 5001）。
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// 扫描码未匹配到可用设备或端口（旧版 ErrScanNotFound）。
    #[error("scan code not found")]
    NotFound,
    /// 中央运营状态缺失或读取失败（旧版按非 NotFound 错误处理 → 503）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ScanError {
    fn from(e: sqlx::Error) -> Self {
        ScanError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 端口视图（旧版 ScanPort：port_id 是端口二维码编码，不是自增主键）。
#[derive(Debug, Serialize)]
pub struct ScanPortView {
    /// 端口二维码编码。
    pub port_id: String,
    /// 设备业务编号。
    pub device_id: String,
    /// 协议端口序号（从 1 开始）。
    pub port_no: u8,
    /// 端口业务状态（device_port.status）。
    pub port_status: String,
    /// 按可见时间窗口判断的在线状态（晚于当前最多 1 分钟、早于最多 2 分钟）。
    pub online: bool,
    /// 空闲、无订单占用且在线的可用状态。
    pub available: bool,
    /// 中央运营状态（device_meta.status；旧版 ApplyDeviceStatus 补充）。
    pub device_status: Option<String>,
}

/// 扫码解析结果（旧版 ScanResult：端口码命中用 port，设备号命中用 ports）。
#[derive(Debug, Serialize)]
pub struct ScanResultView {
    /// 扫描对象种类：port 或 device。
    pub kind: String,
    /// 设备业务编号。
    pub device_id: String,
    /// 站点编号（可空映射；缺失时旧版输出 0）。
    pub station_id: u64,
    /// 中央运营状态。
    pub device_status: Option<String>,
    /// 端口码命中的单端口结果。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<ScanPortView>,
    /// 设备号命中的端口集合。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ScanPortView>>,
}

/// 端口与设备联表行。
type PortRow = (
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
     WHERE `p`.`deleted_at` IS NULL AND `d`.`status` = 'enabled' AND `d`.`deleted_at` IS NULL \
       AND `v`.`status` = 'enabled' AND `v`.`deleted_at` IS NULL";

const SCAN_COLS: &str = "SELECT `p`.`port_code`, `p`.`device_id`, `p`.`port_no`, \
         CAST(`p`.`status` AS CHAR), `p`.`current_order_id`, `d`.`last_seen_at`, `d`.`station_id`";

/// 在线与可用判定（旧版 scanPort：心跳晚于当前最多 1 分钟、早于最多 2 分钟）。
fn port_view(
    code: &str,
    device_id: &str,
    port_no: u8,
    status: &str,
    current: Option<&str>,
    seen: Option<chrono::DateTime<chrono::Utc>>,
) -> ScanPortView {
    let now = chrono::Utc::now();
    let online = match seen {
        Some(at) => {
            let delta = now - at;
            delta <= chrono::Duration::minutes(2) && delta >= -chrono::Duration::minutes(1)
        }
        None => false,
    };
    ScanPortView {
        port_id: code.to_string(),
        device_id: device_id.to_string(),
        port_no,
        port_status: status.to_string(),
        online,
        available: status == "idle" && current.is_none() && online,
        device_status: None,
    }
}

/// 读取中央运营状态（旧版 OperationRepo.DeviceStatus：缺失按读取错误处理）。
async fn device_meta_status(pool: &MySqlPool, device_id: &str) -> Result<String, ScanError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `device_meta` \
         WHERE `device_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    status.ok_or_else(|| ScanError::Storage(anyhow::anyhow!("device_meta 缺行: {device_id}")))
}

/// 运营状态补充（旧版 ApplyDeviceStatus：仅物理 available 且运营 enabled 才可用）。
fn apply_status(view: &mut ScanResultView, status: &str) {
    view.device_status = Some(status.to_string());
    let apply = |p: &mut ScanPortView| {
        p.device_status = Some(status.to_string());
        p.available = p.available && status == "enabled";
    };
    if let Some(p) = view.port.as_mut() {
        apply(p);
    }
    if let Some(ports) = view.ports.as_mut() {
        for p in ports.iter_mut() {
            apply(p);
        }
    }
}

/// 解析扫码内容：先按端口编码，未命中再按设备号读取端口列表（旧版 ResolveScan 顺序）。
pub async fn resolve(pool: &MySqlPool, code: &str) -> Result<ScanResultView, ScanError> {
    // 端口码命中：单端口。
    let port_sql = format!("{SCAN_COLS} {SCAN_FROM} AND `p`.`port_code` = ? LIMIT 1");
    if let Some(row) = sqlx::query_as::<_, PortRow>(&port_sql)
        .bind(code)
        .fetch_optional(pool)
        .await?
    {
        let (station_id, port) = row_fact(row);
        let mut view = ScanResultView {
            kind: "port".into(),
            device_id: port.device_id.clone(),
            station_id,
            device_status: None,
            port: Some(port),
            ports: None,
        };
        let status = device_meta_status(pool, &view.device_id).await?;
        apply_status(&mut view, &status);
        return Ok(view);
    }
    // 设备号命中：端口列表按端口号升序。
    let device_sql =
        format!("{SCAN_COLS} {SCAN_FROM} AND `d`.`device_id` = ? ORDER BY `p`.`port_no`");
    let rows = sqlx::query_as::<_, PortRow>(&device_sql)
        .bind(code)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Err(ScanError::NotFound);
    }
    let mut ports = Vec::with_capacity(rows.len());
    let mut station_id = 0u64;
    let mut device_id = String::new();
    for row in rows {
        let (sid, port) = row_fact(row);
        if station_id == 0 {
            station_id = sid;
        }
        if device_id.is_empty() {
            device_id = port.device_id.clone();
        }
        ports.push(port);
    }
    let mut view = ScanResultView {
        kind: "device".into(),
        device_id,
        station_id,
        device_status: None,
        port: None,
        ports: Some(ports),
    };
    let status = device_meta_status(pool, &view.device_id).await?;
    apply_status(&mut view, &status);
    Ok(view)
}

/// 拆出站点编号与端口视图。
fn row_fact(row: PortRow) -> (u64, ScanPortView) {
    let (code, device_id, port_no, status, current, seen, station_id) = row;
    (
        station_id.unwrap_or(0),
        port_view(
            &code,
            &device_id,
            port_no,
            &status,
            current.as_deref(),
            seen,
        ),
    )
}
