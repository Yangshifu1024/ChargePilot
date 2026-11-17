//! payment 域错误类型（蓝图第五章：域内错误词表集中收口）。

use chargepilot_domain::payment::IntentError;

/// 下单域错误（HTTP 层映射CheckoutError 的 Kind 族）。
/// 下单与支付准备错误（语义分类 → HTTP 映射在 views/payment 门面）。
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    /// 输入不合法。
    #[error("请求参数无效")]
    InvalidInput,
    /// 端口或设备不存在。
    #[error("设备或端口不存在")]
    NotFound,
    /// 设备已停用。
    #[error("设备已停用")]
    Paused,
    /// 端口不可用（被预占或非空闲）。
    #[error("端口已被占用")]
    PortBusy,
    /// 套餐不可售。
    #[error("套餐不可售")]
    OfferUnavailable,
    /// 计费方案不可用或与设备能力不兼容。
    #[error("计费方案不可用")]
    Pricing,
    /// 支付申请冲突（并发或重放不一致）。
    #[error("支付申请冲突")]
    Conflict,
    /// 渠道不可用。
    #[error("支付渠道不可用")]
    Provider,
    /// 存储错误。
    #[error("支付存储错误: {0}")]
    Storage(String),
}
impl From<sqlx::Error> for CheckoutError {
    fn from(e: sqlx::Error) -> Self {
        CheckoutError::Storage(e.to_string())
    }
}
impl From<ScanError> for CheckoutError {
    fn from(e: ScanError) -> Self {
        match e {
            ScanError::NotFound => CheckoutError::NotFound,
            ScanError::Gateway(m) => CheckoutError::Storage(m),
        }
    }
}
impl From<IntentError> for CheckoutError {
    fn from(e: IntentError) -> Self {
        match e {
            IntentError::Conflict => CheckoutError::Conflict,
            IntentError::OfferUnavailable => CheckoutError::OfferUnavailable,
            IntentError::InvalidPricing => CheckoutError::Pricing,
        }
    }
}

/// 扫码解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// 编码未命中任何端口或设备。
    NotFound,
    /// 网关或存储不可用。
    Gateway(String),
}
impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::NotFound => f.write_str("二维码无效"),
            ScanError::Gateway(m) => write!(f, "设备网关不可用: {m}"),
        }
    }
}
impl std::error::Error for ScanError {}

/// 模拟确认错误（HTTP 层映射Err 族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulationError {
    /// 支付单不存在或归属不符。
    NotFound,
    /// 非模拟渠道的支付。
    NotSimulation,
    /// 处理冲突。
    Conflict,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for SimulationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SimulationError::NotFound => f.write_str("支付单不存在"),
            SimulationError::NotSimulation => f.write_str("支付不是模拟渠道"),
            SimulationError::Conflict => f.write_str("模拟支付冲突"),
            SimulationError::Storage(m) => write!(f, "模拟确认存储错误: {m}"),
        }
    }
}
impl std::error::Error for SimulationError {}
impl From<sqlx::Error> for SimulationError {
    fn from(e: sqlx::Error) -> Self {
        SimulationError::Storage(e.to_string())
    }
}
impl From<ApplyError> for SimulationError {
    fn from(e: ApplyError) -> Self {
        match e {
            ApplyError::Conflict => SimulationError::Conflict,
            ApplyError::Storage(m) => SimulationError::Storage(m),
        }
    }
}

/// 支付处理错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// 报文与既有记录冲突或单据状态不允许（HTTP 409）。
    Conflict,
    /// 存储错误。
    Storage(String),
}
impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Conflict => f.write_str("支付通知冲突"),
            ApplyError::Storage(m) => write!(f, "支付处理存储错误: {m}"),
        }
    }
}
impl std::error::Error for ApplyError {}
impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        ApplyError::Storage(e.to_string())
    }
}
impl From<chargepilot_mod_wallet::WalletError> for ApplyError {
    // WalletError 通配以防跨域新增变体静默漏配（wildcard_enum_match_arm）。
    #[allow(clippy::wildcard_enum_match_arm)]
    fn from(e: chargepilot_mod_wallet::WalletError) -> Self {
        match e {
            chargepilot_mod_wallet::WalletError::RequestConflict => ApplyError::Conflict,
            other => ApplyError::Storage(other.to_string()),
        }
    }
}

/// 卡续购没有匹配到已付余额支付单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFundingConflict;
