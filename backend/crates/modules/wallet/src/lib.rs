//! wallet 域：余额账户、充值入账、流水、退款预占与频次风控。
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；既有顶级模块路径
//! （account/credit/ledger/query/recharge_history/recharge/refund/risk）
//! 全部经 re-export 保持（identity/card/payment/settlement 均按顶级路径调用）。

pub use error::WalletError;

pub mod error;
pub mod repo;
pub mod service;

pub use repo::{account, credit, ledger, query, recharge_history};
pub use service::{recharge, refund, risk};

pub use chargepilot_domain::wallet::{AccountStatus, FreezeStatus, TxnBizType, TxnDirection};

/// 通用页结构（wallet 内部查询使用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
}
