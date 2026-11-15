//! charging 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 停止域错误（HTTP 层映射旧版 Err 族）。
#[derive(Debug, thiserror::Error)]
pub enum StopError {
    /// 订单不存在、不属于该用户或不可停止。
    #[error("订单不存在或不可停止")]
    NotFound,
    /// 端口身份缺失（无成功启动回执）。
    #[error("端口身份缺失")]
    PortIdentity,
    /// 截止点持久化失败。
    #[error("计费截止保存失败: {0}")]
    CutoffUnavailable(String),
    /// 命令下发失败（可重试）。
    #[error("停止命令下发失败: {0}")]
    SendFailed(String),
    /// 存储错误。
    #[error("停止处理存储错误: {0}")]
    Storage(String),
}
impl From<sqlx::Error> for StopError {
    fn from(e: sqlx::Error) -> Self {
        StopError::Storage(e.to_string())
    }
}

/// 回执冲突（旧版 ErrStartResultConflict / ErrEndResultConflict）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptError {
    /// 回执与订单、启动回执或计量记录不符。
    Conflict,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiptError::Conflict => f.write_str("回执与订单状态不符"),
            ReceiptError::Storage(m) => write!(f, "回执存储错误: {m}"),
        }
    }
}
impl std::error::Error for ReceiptError {}
impl From<sqlx::Error> for ReceiptError {
    fn from(e: sqlx::Error) -> Self {
        ReceiptError::Storage(e.to_string())
    }
}
impl From<chargepilot_mod_order::OrderError> for ReceiptError {
    fn from(e: chargepilot_mod_order::OrderError) -> Self {
        match e {
            chargepilot_mod_order::OrderError::NumberConflict => ReceiptError::Conflict,
            other => ReceiptError::Storage(other.to_string()),
        }
    }
}
