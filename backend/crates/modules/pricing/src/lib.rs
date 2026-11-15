//! 业务域模块：pricing（计费方案三层模型：模板 → 发布规则 → 生效匹配）。
//! 跨域依赖（票 21 审计）：ops——方案变更的操作员审计（ops）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 语义基准：旧仓库 internal/pricing；发布幂等与版本基线见模块内注释。

pub use error::PricingError;

pub mod error;
pub mod metering;
pub mod publication;
pub mod templates;

pub type PricingResult<T> = Result<T, PricingError>;

pub use publication::SpecWrapper;
