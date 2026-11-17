//! marketing 域：优惠券模板、人工/活动发放与核销。
//! 跨域依赖：identity,ops——持券用户身份事实（identity）；
//! 操作员审计事务（ops）。
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；re-export 保持
//! 既有顶级模块路径不变（coupon/activity/query）。

pub use error::{CouponError, CouponQueryError, CouponRedeemError};

pub mod error;
pub mod repo;
pub mod service;

pub use repo::query;
pub use service::{activity, coupon};
