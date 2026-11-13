//! 扫码解析端口（旧版 gatewaycontract.ResolveScan 的窄接口与直查桩）。
//! 阶段 06 前直查 device_port/device 表；接入网关后由 cmd 装配替换实现，
//! checkout 只依赖本 trait，不感知数据来源。

use super::channel::futures_bounds::BoxFuture;
use sqlx::MySqlPool;

/// 扫码解析出的端口事实（旧版 money.ScanResult 的端口部分）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScanPort {
    /// 端口二维码唯一编码（业务对外 ID）。
    pub port_id: String,
    /// 设备全局唯一编号。
    pub device_id: String,
    /// 端口号，从 1 开始。
    pub port_no: u8,
    /// 端口当前是否可用（idle 且未删除）。
    pub available: bool,
}

/// 扫码解析结果（旧版 money.ScanResult）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScanResult {
    /// 解析种类：port / device / unknown（旧版字面量）。
    pub kind: String,
    pub device_id: String,
    /// 设备业务状态：enabled / disabled / retired。
    pub device_status: String,
    pub station_id: u64,
    pub port: Option<ScanPort>,
}

/// 扫码解析错误（旧版 gatewaycontract.ErrScanNotFound 语义族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// 编码未命中任何端口或设备。
    NotFound,
    /// 网关或存储不可用。
    Gateway(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::NotFound => f.write_str("二维码无效"),
            ScanError::Gateway(m) => write!(f, "设备网关不可用: {m}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// 端口扫描器（checkout 依赖的窄接口）。
pub trait PortScanner: Send + Sync {
    fn resolve_scan<'a>(&'a self, code: &'a str) -> BoxFuture<'a, Result<ScanResult, ScanError>>;
}

/// 直查桩：按端口二维码读 device_port 与 device 表（阶段 06 替换为网关客户端）。
/// 旧版未配置网关（Ports == nil）时 checkout 拒绝创建支付；cmd 装配时按配置
/// 选择本实现或网关客户端。
#[derive(Debug, Clone)]
pub struct TablePortScanner {
    pool: MySqlPool,
}

impl TablePortScanner {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

impl PortScanner for TablePortScanner {
    fn resolve_scan<'a>(&'a self, code: &'a str) -> BoxFuture<'a, Result<ScanResult, ScanError>> {
        Box::pin(async move {
            if code.is_empty() {
                return Err(ScanError::NotFound);
            }
            let row: Option<(String, u8, String, u64, Option<String>)> = sqlx::query_as(
                "SELECT p.`device_id`, p.`port_no`, d.`status`, d.`station_id`, p.`status` \
                 FROM `device_port` p JOIN `device` d ON d.`device_id` = p.`device_id` \
                 WHERE p.`port_code` = ? AND p.`deleted_at` IS NULL AND d.`deleted_at` IS NULL LIMIT 1",
            )
            .bind(code)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ScanError::Gateway(e.to_string()))?;
            let Some((device_id, port_no, device_status, station_id, port_status)) = row else {
                return Err(ScanError::NotFound);
            };
            let available = port_status.as_deref() == Some("idle");
            Ok(ScanResult {
                kind: "port".to_string(),
                device_id: device_id.clone(),
                device_status,
                station_id,
                port: Some(ScanPort {
                    port_id: code.to_string(),
                    device_id,
                    port_no,
                    available,
                }),
            })
        })
    }
}
