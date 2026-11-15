//! 扫码解析端口（旧版 gatewaycontract.ResolveScan 的窄接口与直查桩）。
//! checkout 只依赖本 trait，不感知数据来源；直查桩的 SQL 在 repo/scan.rs。

use super::channel::futures_bounds::BoxFuture;
use crate::error::ScanError;

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

/// 端口扫描器（checkout 依赖的窄接口）。
pub trait PortScanner: Send + Sync {
    fn resolve_scan<'a>(&'a self, code: &'a str) -> BoxFuture<'a, Result<ScanResult, ScanError>>;
}

pub use crate::repo::scan::TablePortScanner;
