//! 业务域模块：wallet（钱包）。
//! 职责：账户余额与流水账本、充值申请与回调入账、余额退款申请与原路拆单预占、
//! 风险审核与解冻。表归属：wallet_account、wallet_txn、wallet_recharge_request、
//! wallet_refund_request、wallet_refund_part、wallet_risk_freeze_link、
//! wallet_risk_release、wallet_risk_review、risk_freeze_log（docs/design/schema.md）。
//!
//! 语义基准：旧仓库 internal/wallet/*（账本/申请/预占/风控）与
//! internal/domain/{money,wallet}/*（词表）。资金写入全部要求调用方持有钱包行锁；
//! 锁序约定：充值申请为 用户→钱包→请求，支付回调为 支付→申请→钱包，
//! 退款预占为 钱包→支付→在途退款。

pub mod account;
pub mod credit;
pub mod ledger;
pub mod query;
pub mod recharge;
pub mod recharge_history;
pub mod refund;
pub mod risk;

/// wallet_account.status 列的账户状态（active/frozen，旧版 walletdomain.AccountStatus）。
/// 与 identity 的 UserStatus 分立：取值串相同但描述不同事实，不得互相赋值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Frozen,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Active => "active",
            AccountStatus::Frozen => "frozen",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "active" => Some(AccountStatus::Active),
            "frozen" => Some(AccountStatus::Frozen),
            _ => None,
        }
    }
}

/// wallet_txn.direction 列的资金方向（in/out）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnDirection {
    In,
    Out,
}

impl TxnDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            TxnDirection::In => "in",
            TxnDirection::Out => "out",
        }
    }
}

/// wallet_txn.biz_type 列的流水业务类型（七值全词表）。
/// gift/freeze/unfreeze/adjust 当前无写入路径，保留以完整覆盖列约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnBizType {
    Recharge,
    Pay,
    Refund,
    Gift,
    Freeze,
    Unfreeze,
    Adjust,
}

impl TxnBizType {
    pub fn as_str(self) -> &'static str {
        match self {
            TxnBizType::Recharge => "recharge",
            TxnBizType::Pay => "pay",
            TxnBizType::Refund => "refund",
            TxnBizType::Gift => "gift",
            TxnBizType::Freeze => "freeze",
            TxnBizType::Unfreeze => "unfreeze",
            TxnBizType::Adjust => "adjust",
        }
    }
}

/// risk_freeze_log.status 列的冻结记录状态（frozen/unfrozen）。
/// 与 AccountStatus 分立：本类型描述「一条冻结记录处于什么阶段」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeStatus {
    Frozen,
    Unfrozen,
}

impl FreezeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FreezeStatus::Frozen => "frozen",
            FreezeStatus::Unfrozen => "unfrozen",
        }
    }
}

/// 钱包域错误：变体与旧版哨兵一一对应，HTTP 映射由 http-api 层完成。
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// 依赖缺失（数据库未配置等）。
    #[error("storage failure: {0}")]
    Storage(String),
    /// 实时权限不足（旧版 operations.ErrForbidden 的钱包侧映射）。
    #[error("forbidden")]
    Forbidden,
    /// 钱包当前不可发起消费或新退款（旧版 ErrFrozen）。
    #[error("wallet is frozen")]
    Frozen,
    /// 可用余额不足（旧版 ErrInsufficientBalance / ErrInsufficient）。
    #[error("insufficient available wallet balance")]
    InsufficientBalance,
    /// 无有效渠道用户身份（旧版 ErrIdentity）。
    #[error("wallet owner identity unavailable")]
    Identity,
    /// 同请求号的申请人或金额不同（充值与退款共用，旧版两个 Conflict 哨兵）。
    #[error("wallet request conflicts")]
    RequestConflict,
    /// 申请已保存但未配置渠道（旧版 ErrPrepayNotConfigured）。
    #[error("wallet recharge provider not configured")]
    PrepayNotConfigured,
    /// 渠道准备失败，保留支付单供相同请求重试（旧版 ErrPrepayUnavailable）。
    #[error("wallet recharge prepay unavailable")]
    PrepayUnavailable,
    /// 对非 active 状态钱包出账（旧版 ErrInactive）。
    #[error("wallet account is not active for debit")]
    Inactive,
    /// 余额、冻结额或变动金额不满足账本范围（旧版 ErrInvalidBalance）。
    #[error("invalid wallet balance or movement")]
    InvalidBalance,
    /// 无记录（旧版 gorm.ErrRecordNotFound 的调用方判定位）。
    #[error("record not found")]
    NotFound,
}

impl From<sqlx::Error> for WalletError {
    // sqlx::Error 标记 non_exhaustive，通配不可避免（wildcard_enum_match_arm）。
    #[allow(clippy::wildcard_enum_match_arm)]
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => WalletError::NotFound,
            _ => WalletError::Storage(e.to_string()),
        }
    }
}

impl From<anyhow::Error> for WalletError {
    fn from(e: anyhow::Error) -> Self {
        WalletError::Storage(e.to_string())
    }
}

/// 通用页结构（与旧版 persist.Page 对齐；wallet 内部查询使用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
}
