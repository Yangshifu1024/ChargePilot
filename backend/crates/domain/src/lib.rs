//! 纯业务规则与跨用例契约：零 IO、零框架依赖（不出现 axum/sqlx/redis 类型）。
//!
//! 组织：按域子目录归置（charging/money/card/payment/...），全域共享的
//! 界限值、分页校验与词表宏留在顶层；模块树基准见 docs/blueprint.md 第三章。

pub mod card;
pub mod charging;
pub mod device;
pub mod export;
pub mod identity;
pub mod limits;
pub mod marketing;
pub mod money;
pub mod operations;
pub mod order;
pub mod page;
pub mod payment;
pub mod pricing;
pub mod settlement;
pub mod telemetry;
pub mod wallet;
pub mod wordlist;
