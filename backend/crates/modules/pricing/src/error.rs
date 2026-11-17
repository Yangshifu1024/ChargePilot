//! pricing 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// pricing 域错误（HTTP 映射在 http-api）。
#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    /// 方案校验失败或目标设备不兼容。
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
