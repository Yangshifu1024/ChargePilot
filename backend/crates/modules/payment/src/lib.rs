//! 业务域模块：payment（支付意图、渠道、支付成功处理与来源锁定）。
//! 跨域依赖（票 21 审计）：marketing,order,pricing,wallet——券核销幂等（marketing）；订单号分配与订单事实（order）；计费方案扫描与冻结（pricing）；余额充值桥与钱包错误映射（wallet）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 表归属：payment_order、charge_payment_intent、charge_prepay、
//! payment_callback_idempotent（docs/design/schema.md）。
//!
//! 语义基准：旧仓库 internal/payment/*（checkout/callback/channel）与
//! internal/domain/money（词表）。支付成功事务是充电状态机的入口（ADR 002）：
//! 同事务推进支付单、意图、充电订单与出站事件；回调与模拟确认共用入口。
//! 锁序：支付单 → 意图 →（钱包充值）申请与钱包，与退款预占的 钱包→支付 相序
//! 不同属不同事务路径，评审时逐条核对不得混用。

pub use error::{ApplyError, CardFundingConflict, CheckoutError, ScanError, SimulationError};

pub mod apply;
pub mod card_funding;
pub mod channel;
pub mod checkout;
pub mod error;
pub mod recharge_bridge;
pub mod scan;
pub mod simulation;
pub mod sources;
pub mod wechat;
#[cfg(test)]
mod wechat_test;

pub use channel::{PayMode, PayProvider, PrepayParams, PrepayRequest};
pub use checkout::{CheckoutInput, CheckoutOffers, CheckoutResult, CheckoutService};
pub use scan::{PortScanner, ScanResult};
