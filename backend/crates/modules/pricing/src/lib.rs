//! pricing 域：计费方案三层模型（模板 → 发布规则 → 生效匹配）。
//! 跨域依赖（票 21 审计）：ops——方案变更的操作员审计。
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；下面的 re-export
//! 保持既有顶级模块路径不变（metering/templates/publication、SpecWrapper）。

pub use error::PricingError;

pub mod error;
pub mod repo;
pub mod service;

pub type PricingResult<T> = Result<T, PricingError>;

pub use repo::metering;
pub use service::publication::SpecWrapper;
pub use service::{publication, templates};
