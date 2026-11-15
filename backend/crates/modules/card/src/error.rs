//! card 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 刷卡域错误（旧版 SwipeError 阶段 + 哨兵语义）。HTTP/网关把
/// CardDeclined / CardBalance / Conflict 映射为 409 + 业务码 2009。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwipeError {
    /// 输入不合法（卡号、事件号或端口编码）。
    Input,
    /// 事件超时或来自未来。
    Expired,
    /// 端口解析失败或离线。
    Port,
    /// 设备停用。
    Paused,
    /// 协议不支持在线卡。
    Unsupported,
    /// 方案缺失或无效，或卡套餐未配置。
    Pricing,
    /// 端口已有未完成支付或占用。
    Conflict,
    /// 卡状态、余额、端口、累计上限或订单状态不允许刷卡（2009 族）。
    CardDeclined,
    /// 钱包可用余额不足（2009 族）。
    CardBalance,
    /// 存储错误。
    Persist(String),
}
impl std::fmt::Display for SwipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwipeError::Input => f.write_str("刷卡请求参数无效"),
            SwipeError::Expired => f.write_str("刷卡事件已超时"),
            SwipeError::Port => f.write_str("充电端口不可用"),
            SwipeError::Paused => f.write_str("设备已停用"),
            SwipeError::Unsupported => f.write_str("设备不支持在线卡"),
            SwipeError::Pricing => f.write_str("充电方案不可用"),
            SwipeError::Conflict => f.write_str("端口存在进行中的支付或充电"),
            SwipeError::CardDeclined => {
                f.write_str("刷卡不可用：卡状态、余额、端口、累计上限或设备确认状态不满足要求")
            }
            SwipeError::CardBalance => f.write_str("刷卡不可用：钱包可用余额不足"),
            SwipeError::Persist(m) => write!(f, "刷卡处理存储错误: {m}"),
        }
    }
}
impl std::error::Error for SwipeError {}
impl From<sqlx::Error> for SwipeError {
    fn from(e: sqlx::Error) -> Self {
        SwipeError::Persist(e.to_string())
    }
}
impl SwipeError {
    /// 是否 409/2009 拒绝族（明确拒绝，可安全应答设备；网络不确定不算）。
    pub fn is_declined(&self) -> bool {
        matches!(
            self,
            SwipeError::CardDeclined | SwipeError::CardBalance | SwipeError::Conflict
        )
    }
}
impl SwipeError {
    /// 号段分配错误（纯存储失败）。
    pub(crate) fn from_anyhow(e: anyhow::Error) -> SwipeError {
        SwipeError::Persist(e.to_string())
    }
}

/// 后台在线卡错误（HTTP 层翻译状态码）。
#[derive(Debug, thiserror::Error)]
pub enum CardAdminError {
    /// 绑定或状态输入不满足业务约束（400，文案由处理器按端点给出）。
    #[error("card admin input invalid")]
    Input,
    /// 实时权限不足（403/1003）。
    #[error("当前账号无权执行此操作")]
    Forbidden,
    /// 记录当前状态拒绝写入（409，文案透出）。
    #[error("{0}")]
    Conflict(String),
    /// 卡或目标用户不存在（404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储不可用（503/5003）。
    #[error("存储不可用: {0}")]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for CardAdminError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 用户卡状态操作错误（HTTP 映射沿用旧版字面量契约）。
#[derive(Debug, thiserror::Error)]
pub enum UserCardError {
    /// 状态不在 C 端可写子集（lost/unbound 之外，400/1005）。
    #[error("card status input invalid")]
    Input,
    /// 卡不存在、属于他人或已解绑，统一 404 不泄露归属（404/1004）。
    #[error("card not found")]
    NotFound,
    /// 存储错误（503/5001）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for UserCardError {
    fn from(e: sqlx::Error) -> Self {
        UserCardError::Storage(anyhow::anyhow!(e.to_string()))
    }
}
