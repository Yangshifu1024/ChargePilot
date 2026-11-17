//! export 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 导出域错误。
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// 请求参数不合法（400/1005，文案透出）。
    #[error("{0}")]
    Input(String),
    /// 权限、归属或范围不足（403/1003，文案透出）。
    #[error("{0}")]
    Forbidden(String),
    /// 登录凭据已变化（401）。
    #[error("credentials invalid")]
    Credentials,
    /// 当前状态不允许下载；过期返回 410，其余 409。
    #[error("{message}")]
    Download { message: String, expired: bool },
    /// 记录不存在（404）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 冲突（409/2009）。
    #[error("导出请求冲突")]
    Conflict,
    /// 存储错误（503/5003）。
    #[error("导出存储错误: {0}")]
    Storage(String),
}
impl From<sqlx::Error> for ExportError {
    fn from(e: sqlx::Error) -> Self {
        ExportError::Storage(e.to_string())
    }
}
impl From<anyhow::Error> for ExportError {
    fn from(e: anyhow::Error) -> Self {
        ExportError::Storage(e.to_string())
    }
}

/// 校验失败分类：Unavailable→HTTP 409、Forbidden→403（由调用方映射状态码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    /// 文件缺失、路径不符或校验不通过。
    Unavailable,
    /// 当前范围或脱敏规则无权访问该文件。
    Forbidden,
}
