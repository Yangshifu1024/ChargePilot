//! 匿名扫码解析用例：先按端口编码查询，再按设备号读取端口列表；
//! 中央运营状态补充可用性。本查询不创建支付或会话；
//! 码值校验（裸码字符集与 HTTPS 链接提取）由 HTTP 层完成。SQL 在 repo/scan.rs。

use crate::ScanError;
use crate::repo::scan;
use serde::Serialize;
use sqlx::MySqlPool;

/// 端口视图（port_id 是端口二维码编码，不是自增主键）。
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
    /// 中央运营状态（device_meta.status；运营态补充）。
    pub device_status: Option<String>,
}

/// 扫码解析结果（端口码命中用 port，设备号命中用 ports）。
#[derive(Debug, Serialize)]
pub struct ScanResultView {
    /// 扫描对象种类：port 或 device。
    pub kind: String,
    /// 设备业务编号。
    pub device_id: String,
    /// 站点编号（可空映射；缺失时输出 0）。
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

/// 在线与可用判定：心跳晚于当前最多 1 分钟、早于最多 2 分钟。
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

/// 运营状态补充：仅物理 available 且运营 enabled 才可用。
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

/// 拆出站点编号与端口视图。
fn row_fact(row: scan::PortRow) -> (u64, ScanPortView) {
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

/// 解析扫码内容：先按端口编码，未命中再按设备号读取端口列表。
pub async fn resolve(pool: &MySqlPool, code: &str) -> Result<ScanResultView, ScanError> {
    // 端口码命中：单端口。
    if let Some(row) = scan::port_by_code(pool, code).await? {
        let (station_id, port) = row_fact(row);
        let mut view = ScanResultView {
            kind: "port".into(),
            device_id: port.device_id.clone(),
            station_id,
            device_status: None,
            port: Some(port),
            ports: None,
        };
        let status = scan::device_meta_status(pool, &view.device_id).await?;
        apply_status(&mut view, &status);
        return Ok(view);
    }
    // 设备号命中：端口列表按端口号升序。
    let rows = scan::ports_by_device(pool, code).await?;
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
    let status = scan::device_meta_status(pool, &view.device_id).await?;
    apply_status(&mut view, &status);
    Ok(view)
}
