//! 业务域模块：device（站点、设备台账、厂商、导入、故障、告警）。
//! 阶段 03 批 1：站点 CRUD、厂商 CRUD。设备运行时表（device/telemetry 等）写权
//! 属 gateway-runtime（阶段 06），本模块只写自己的台账表。

pub mod station;
pub mod vendor;

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

pub type StationResult<T> = Result<T, DeviceError>;
pub type VendorResult<T> = Result<T, DeviceError>;

pub mod alert;
pub mod fault;
pub mod import;
pub mod management;
pub mod provision;
pub mod user_station;
