//! 业务域模块：device（站点、设备台账、厂商、导入、故障、告警）。
//! 跨域依赖（票 21 审计）：ops——告警认领操作员与审计事务（ops）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 阶段 03 批 1：站点 CRUD、厂商 CRUD。设备运行时表（device/telemetry 等）写权
//! 属 gateway-runtime（阶段 06），本模块只写自己的台账表。

pub use error::{DeviceError, ScanError};

pub mod error;
pub mod station;
pub mod vendor;

pub type StationResult<T> = Result<T, DeviceError>;
pub type VendorResult<T> = Result<T, DeviceError>;

pub mod alert;
pub mod capabilities;
pub mod fault;
pub mod import;
pub mod management;
pub mod provision;
pub mod scan;
pub mod user_station;
