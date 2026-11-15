//! wallet 域错误类型（蓝图第五章：域内错误词表集中收口）。

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
