//! 业务域模块：wallet（钱包）。
//! 跨域依赖（票 21 审计）：ops——风控操作员归属判定复用 ops 的实时权限语义（审批人须持有 finance 权限），经服务函数复核。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 职责：账户余额与流水账本、充值申请与回调入账、余额退款申请与原路拆单预占、
//! 风险审核与解冻。表归属：wallet_account、wallet_txn、wallet_recharge_request、
//! wallet_refund_request、wallet_refund_part、wallet_risk_freeze_link、
//! wallet_risk_release、wallet_risk_review、risk_freeze_log（docs/design/schema.md）。
//!
//! 语义基准：旧仓库 internal/wallet/*（账本/申请/预占/风控）与
//! internal/domain/{money,wallet}/*（词表）。资金写入全部要求调用方持有钱包行锁；
//! 锁序约定：充值申请为 用户→钱包→请求，支付回调为 支付→申请→钱包，
//! 退款预占为 钱包→支付→在途退款。

pub use error::WalletError;

pub mod account;
pub mod credit;
pub mod error;
pub mod ledger;
pub mod query;
pub mod recharge;
pub mod recharge_history;
pub mod refund;
pub mod risk;

pub use chargepilot_domain::wallet::{AccountStatus, FreezeStatus, TxnBizType, TxnDirection};

/// 通用页结构（与旧版 persist.Page 对齐；wallet 内部查询使用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
}
