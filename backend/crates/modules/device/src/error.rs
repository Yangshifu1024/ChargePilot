//! device 域错误类型（蓝图第五章：域内错误词表集中收口）。

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("{0}")]
    Input(String),
    #[error("没有操作权限")]
    Forbidden,
    #[error("记录不存在或已删除")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("storage failure: {0}")]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for DeviceError {
    fn from(e: sqlx::Error) -> Self {
        DeviceError::Storage(e.into())
    }
}
impl From<serde_json::Error> for DeviceError {
    fn from(e: serde_json::Error) -> Self {
        DeviceError::Storage(e.into())
    }
}

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
