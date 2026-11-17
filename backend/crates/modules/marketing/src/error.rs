//! marketing 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 券视图错误：输入不合法或存储不可用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouponQueryError {
    /// 请求内容不合法（400/1005，文案透出）。
    Input(String),
    /// 存储错误（503/5003）。
    Storage(String),
}
impl std::fmt::Display for CouponQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CouponQueryError::Input(m) => f.write_str(m),
            CouponQueryError::Storage(m) => write!(f, "优惠券查询存储错误: {m}"),
        }
    }
}
impl std::error::Error for CouponQueryError {}
impl From<sqlx::Error> for CouponQueryError {
    fn from(e: sqlx::Error) -> Self {
        CouponQueryError::Storage(e.to_string())
    }
}

/// 券域错误（记录缺失、权限与并发冲突等；HTTP 层按变体转码值）。
#[derive(Debug, thiserror::Error)]
pub enum CouponError {
    /// 请求内容不合法（400/1005，文案透出）。
    #[error("{0}")]
    Input(String),
    /// 权限不足（403/1003）。
    #[error("无权执行此操作")]
    Forbidden,
    /// 发放条件不满足或请求冲突（409/2009，文案透出）。
    #[error("{0}")]
    Conflict(String),
    /// 模板不存在（404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储错误（503/5003）。
    #[error("优惠券存储错误: {0}")]
    Storage(String),
}
impl From<sqlx::Error> for CouponError {
    fn from(e: sqlx::Error) -> Self {
        CouponError::Storage(e.to_string())
    }
}
impl From<anyhow::Error> for CouponError {
    fn from(e: anyhow::Error) -> Self {
        CouponError::Storage(e.to_string())
    }
}

/// 券域错误的营销专用变体：HTTP 下单链路按
/// （NotFound→400、Exhausted→409/2001、Threshold→400），与通用 Conflict 区分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouponRedeemError {
    /// 券不存在、不属于该用户或模板不可用。
    NotFound,
    /// 券已使用、已到期或状态不再为 unused。
    Exhausted,
    /// 原价非正或低于使用门槛。
    Threshold,
    /// 核销参数或冻结优惠与首次不一致。
    Mismatch,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for CouponRedeemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CouponRedeemError::NotFound => f.write_str("优惠券不存在或未发放给该账号"),
            CouponRedeemError::Exhausted => f.write_str("优惠券已过期或已使用"),
            CouponRedeemError::Threshold => f.write_str("未达到该优惠券的使用门槛"),
            CouponRedeemError::Mismatch => f.write_str("优惠券核销信息不一致"),
            CouponRedeemError::Storage(m) => write!(f, "优惠券存储错误: {m}"),
        }
    }
}
impl std::error::Error for CouponRedeemError {}
impl From<sqlx::Error> for CouponRedeemError {
    fn from(e: sqlx::Error) -> Self {
        CouponRedeemError::Storage(e.to_string())
    }
}
