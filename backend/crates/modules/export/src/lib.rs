//! 业务域模块：export（批量导出任务、文件生成与发布、下载校验、到期清理）。
//! 跨域依赖（票 21 审计）：ops——导出申请的操作员归属与审计（ops）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 旧版 internal/export 的单体对应物。核心语义：
//! - 受理即同事务登记（任务号唯一键兜底并发）+ 锁定权限核对（角色集合→
//!   账号→角色→权限码，auth_version 变化即凭据失效）+ 数据范围/脱敏
//!   FOR SHARE 读取，文件与 SQL 不保证原子，下载时以校验文件兜底。
//! - 文件仅按可信任务号命名，数据库路径只用于比对；下载核对校验文件的
//!   任务、范围、脱敏与 SHA-256。
//! - 清理为受管理循环（每小时、每批 ≤1000），语义照搬旧版。

pub mod artifact;
pub mod cleanup;
pub mod registry;
pub mod service;

pub use cleanup::cleanup_expired;
pub use registry::{ExportColumn, ExportSpec, export_resource, export_resource_names};
pub use service::{
    CreateInput, CreateResult, DownloadFile, ExportConfig, ExportError, ExportTaskRow,
    ResourcesResult, create_export, detail, download, list_resources, list_tasks,
};
