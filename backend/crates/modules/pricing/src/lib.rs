//! 业务域模块：pricing（计费方案三层模型：模板 → 发布规则 → 生效匹配）。
//! 跨域依赖（票 21 审计）：ops——方案变更的操作员审计（ops）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 语义基准：旧仓库 internal/pricing；发布幂等与版本基线见模块内注释。

pub mod metering;
pub mod publication;
pub mod templates;

/// pricing 域错误（HTTP 映射在 http-api）。
#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    /// 方案校验失败或目标设备不兼容（409/2009，旧版包装为冲突）。
    #[error("{0}")]
    InvalidScheme(String),
    /// 版本冲突（409/2009）。
    #[error("方案已被他人修改，请刷新后重试")]
    VersionConflict,
    /// 无可用生效规则（下单时映射为业务不可用）。
    #[error("该端口暂无可用的计费方案")]
    RuleUnavailable,
    /// 站点/厂商超出数据范围（403/1003）。
    #[error("没有操作权限")]
    Forbidden,
    /// 记录不存在（404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 发布幂等冲突：同 request_id 但内容或操作者不同（409/2009）。
    #[error("发布请求与首次提交不一致，请刷新页面后重试")]
    PublishMismatch,
    /// 存储不可用（503/5003）。
    #[error("storage failure: {0}")]
    Storage(#[from] anyhow::Error),
}

pub type PricingResult<T> = Result<T, PricingError>;

pub use publication::SpecWrapper;

impl From<sqlx::Error> for PricingError {
    fn from(e: sqlx::Error) -> Self {
        PricingError::Storage(e.into())
    }
}

impl From<serde_json::Error> for PricingError {
    fn from(e: serde_json::Error) -> Self {
        PricingError::Storage(e.into())
    }
}
