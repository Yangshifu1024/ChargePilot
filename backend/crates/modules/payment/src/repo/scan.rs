//! 扫码直查桩的表访问：device_port / device（蓝图第五章 repo 层）。
//! 网关未配置时直查表；接入网关后由 cmd 装配替换为网关客户端。

use crate::error::ScanError;
use crate::service::scan::{ScanPort, ScanResult};
use sqlx::MySqlPool;

/// 扫码行投影（端口、设备状态与站点归属）。
pub type ScanPortRow = (String, u8, String, u64, Option<String>);

/// 按端口二维码读 device_port 与 device 表。
pub async fn scan_port_row(
    pool: &MySqlPool,
    code: &str,
) -> Result<Option<ScanPortRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.`device_id`, p.`port_no`, d.`status`, COALESCE(d.`station_id`, CAST(0 AS UNSIGNED)) AS `station_id`, p.`status` \
         FROM `device_port` p JOIN `device` d ON d.`device_id` = p.`device_id` \
         WHERE p.`port_code` = ? AND p.`deleted_at` IS NULL AND d.`deleted_at` IS NULL LIMIT 1",
    )
    .bind(code)
    .fetch_optional(pool)
    .await
}

/// 直查桩：checkout 依赖的 PortScanner 唯一生产实现（cmd 装配时按配置选择）。
#[derive(Debug, Clone)]
pub struct TablePortScanner {
    pool: MySqlPool,
}

impl TablePortScanner {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

impl crate::service::scan::PortScanner for TablePortScanner {
    fn resolve_scan<'a>(
        &'a self,
        code: &'a str,
    ) -> crate::service::channel::futures_bounds::BoxFuture<'a, Result<ScanResult, ScanError>> {
        Box::pin(async move {
            if code.is_empty() {
                return Err(ScanError::NotFound);
            }
            let Some((device_id, port_no, device_status, station_id, port_status)) =
                scan_port_row(&self.pool, code)
                    .await
                    .map_err(|e| ScanError::Gateway(e.to_string()))?
            else {
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
