//! device 域：站点、设备台账、厂商、导入、故障、告警。
//! 跨域依赖：ops——告警认领操作员与审计事务（ops）。
//! 设备运行时表（device/telemetry 等）写权属 gateway-runtime，本模块只写台账表。
//!
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问与读模型；
//! re-export 保持既有顶级模块路径不变（station/vendor/alert/scan 等）。

pub use error::{DeviceError, ScanError};

pub mod error;
pub mod repo;
pub mod service;

pub type StationResult<T> = Result<T, DeviceError>;
pub type VendorResult<T> = Result<T, DeviceError>;

pub use repo::{capabilities, user_station};
pub use service::{alert, fault, import, management, scan, station, vendor};

pub mod provision;
