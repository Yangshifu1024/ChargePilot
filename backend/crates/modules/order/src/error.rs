//! order 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 反馈业务错误（HTTP 层映射：Input→400、Conflict→409、NotFound→404、Forbidden→403）。
#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    /// 业务输入错误（文案透出）。
    #[error("{0}")]
    Input(String),
    /// 状态流转冲突（已关闭或非 pending 回复）。
    #[error("反馈状态已变化，请刷新后重试")]
    Conflict,
    /// 反馈不存在或已删除。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 实时权限不足。
    #[error("没有操作权限")]
    Forbidden,
    /// 存储错误。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for FeedbackError {
    fn from(e: sqlx::Error) -> Self {
        FeedbackError::Storage(anyhow::anyhow!(e.to_string()))
    }
}
impl From<OrderError> for FeedbackError {
    fn from(e: OrderError) -> Self {
        FeedbackError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 用户提交反馈错误。
#[derive(Debug, thiserror::Error)]
pub enum FeedbackSubmitError {
    /// 订单引用或反馈字段校验失败（400/1005，文案透出）。
    #[error("{0}")]
    Input(String),
    /// 订单缺失、属于他人或状态不可评价（404/1004 订单不存在或不可评价）。
    #[error("order is not a completed order of this user")]
    OrderNotReady,
    /// 订单已评价，重复提交为冲突（409/2009 该订单已评价）。
    #[error("order already has feedback")]
    Exists,
    /// 按业务单号解析时订单不存在（404/1004 订单不存在）。
    #[error("order not found")]
    NotFound,
    /// 存储错误（503/5003 反馈提交失败，请稍后重试）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for FeedbackSubmitError {
    fn from(e: sqlx::Error) -> Self {
        FeedbackSubmitError::Storage(anyhow::anyhow!(e.to_string()))
    }
}
impl From<serde_json::Error> for FeedbackSubmitError {
    fn from(e: serde_json::Error) -> Self {
        FeedbackSubmitError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 订单域错误（支付成功事务中的冲突语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderError {
    /// 业务单号冲突（同秒同设备同端口重复或编号超限）。
    NumberConflict,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::NumberConflict => f.write_str("充电单号冲突"),
            OrderError::Storage(m) => write!(f, "订单存储错误: {m}"),
        }
    }
}
impl std::error::Error for OrderError {}
impl From<sqlx::Error> for OrderError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            // 23000（MySQL 1062）：order_no 唯一键冲突（uk_order_no 按分区键组合唯一）。
            if db.code().as_deref() == Some("23000") {
                return OrderError::NumberConflict;
            }
        }
        OrderError::Storage(e.to_string())
    }
}
