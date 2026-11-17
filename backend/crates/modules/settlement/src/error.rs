//! settlement 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 用户端发票域错误；HTTP 映射在路由层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInvoiceError {
    /// 申请内容无效（正常请求已被 HTTP 层前置校验）。
    InvalidInput,
    /// 本人订单不存在。
    OrderNotFound,
    /// 订单读取故障（事务内锁定读取失败）。
    OrderLookup(String),
    /// 订单尚未完成，暂不能申请。
    Unfinished,
    /// 计费回执缺失。
    Unbilled,
    /// 已确认费用未确定或非正数。
    ZeroFee,
    /// 订单已有申请，或同号申请的身份与内容不一致。
    Duplicate,
    /// 存储错误。
    Storage(String),
}

/// 发票审核域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoiceAdminError {
    /// 发票申请不存在或已删除。
    NotFound,
    /// 状态、签名或内容冲突。
    Conflict,
    /// 服务内实时权限复核失败（正常请求已被 HTTP 层拦截）。
    Forbidden,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for InvoiceAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvoiceAdminError::NotFound => f.write_str("发票申请不存在"),
            InvoiceAdminError::Conflict => f.write_str("发票审核状态冲突"),
            InvoiceAdminError::Forbidden => f.write_str("当前账号无权执行此操作"),
            InvoiceAdminError::Storage(m) => write!(f, "发票审核存储错误: {m}"),
        }
    }
}
impl std::error::Error for InvoiceAdminError {}
impl From<sqlx::Error> for InvoiceAdminError {
    fn from(e: sqlx::Error) -> Self {
        InvoiceAdminError::Storage(e.to_string())
    }
}

/// 人工复核域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    /// 请求冲突：状态、内容或审核人不允许该操作。
    Conflict,
    /// 计量或快照无效。
    InvalidSource,
    /// 记录不存在。
    NotFound,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewError::Conflict => f.write_str("人工复核请求冲突"),
            ReviewError::InvalidSource => f.write_str("计量或计费快照无效"),
            ReviewError::NotFound => f.write_str("记录不存在"),
            ReviewError::Storage(m) => write!(f, "人工复核存储错误: {m}"),
        }
    }
}
impl std::error::Error for ReviewError {}
impl From<sqlx::Error> for ReviewError {
    fn from(e: sqlx::Error) -> Self {
        ReviewError::Storage(e.to_string())
    }
}

/// 人工退款域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundAdminError {
    /// 请求冲突：内容不一致、状态不允许或金额超限。
    Conflict,
    /// 已结束充电尚无费用回执。
    NotSettled,
    /// 退款单不存在。
    NotFound,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for RefundAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefundAdminError::Conflict => f.write_str("人工退款请求冲突"),
            RefundAdminError::NotSettled => f.write_str("实际结束的充电尚无费用回执"),
            RefundAdminError::NotFound => f.write_str("退款单不存在"),
            RefundAdminError::Storage(m) => write!(f, "人工退款存储错误: {m}"),
        }
    }
}
impl std::error::Error for RefundAdminError {}
impl From<sqlx::Error> for RefundAdminError {
    fn from(e: sqlx::Error) -> Self {
        RefundAdminError::Storage(e.to_string())
    }
}