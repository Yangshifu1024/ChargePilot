//! 统一错误码表与业务错误类型。
//!
//! 码值契约（红线）：1xxx 请求或凭证、2xxx 业务状态冲突、5xxx 服务端或依赖故障；
//! 数值与旧版完全一致（前端按 code 映射文案），一经发布不得变更。

/// 业务错误码（数值契约锁定，禁止改动已有取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Code {
    /// 未认证或凭证无效（HTTP 401）。
    Unauthorized = 1001,
    /// 验证码、MFA 或第二签等校验未通过（HTTP 400）。
    VerificationFailed = 1002,
    /// 无权访问、账号冻结或超出数据范围（HTTP 403）。
    Forbidden = 1003,
    /// 资源不存在（HTTP 404）。
    NotFound = 1004,
    /// 参数缺失或非法（HTTP 400）。
    BadRequest = 1005,
    /// 请求方法不被支持（HTTP 405）。
    MethodNotAllowed = 1006,
    /// 资源已过期，如导出下载链接（HTTP 410）。
    ResourceExpired = 1007,
    /// 批量导入内容与既有配置冲突（HTTP 409）。
    ImportConflict = 1009,
    /// 订单、指令或回执与当前状态冲突（HTTP 409）。
    StateConflict = 2000,
    /// 业务资源当前不可用：设备暂停、端口占用、支付意图失效（HTTP 409）。
    ResourceUnavailable = 2001,
    /// 计费方案不可用或已下架（HTTP 409）。
    PricingUnavailable = 2004,
    /// 账号保护锁定（HTTP 423）。
    AccountLocked = 2006,
    /// 业务规则冲突：重复申请、状态已变化、身份或余额冲突（HTTP 409）。
    Conflict = 2009,
    /// 定时任务状态不允许触发（HTTP 409）。
    TaskConflict = 2010,
    /// 业务依赖失败：会话、用户、支付、卡或网关调用（HTTP 503）。
    DependencyFailed = 5001,
    /// 存储、队列或上游接口不可用（HTTP 503）。
    ServiceUnavailable = 5003,
}

impl Code {
    /// 对应 HTTP 状态码（信封语义的组成部分）。
    pub fn http_status(self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Code::Unauthorized => StatusCode::UNAUTHORIZED,
            Code::VerificationFailed | Code::BadRequest => StatusCode::BAD_REQUEST,
            Code::Forbidden => StatusCode::FORBIDDEN,
            Code::NotFound => StatusCode::NOT_FOUND,
            Code::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Code::ResourceExpired => StatusCode::GONE,
            Code::ImportConflict
            | Code::StateConflict
            | Code::ResourceUnavailable
            | Code::PricingUnavailable
            | Code::Conflict
            | Code::TaskConflict => StatusCode::CONFLICT,
            Code::AccountLocked => StatusCode::LOCKED,
            Code::DependencyFailed | Code::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// 携带码值与用户文案的业务错误；内部原因不进响应（走 tracing）。
#[derive(Debug, thiserror::Error)]
#[error("[{code:?}] {message}")]
pub struct ApiError {
    pub code: Code,
    pub message: String,
    /// 字段级校验错误（可选，仅参数校验类错误使用）。
    pub fields: Vec<FieldErr>,
    /// 内部原因（日志用，不进响应）。
    pub cause: Option<anyhow::Error>,
}

/// 参数名与未满足的校验规则（信封 errors 数组元素）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldErr {
    pub field: String,
    pub rule: String,
}

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fields: Vec::new(),
            cause: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::NotFound, message)
    }

    pub fn state_conflict(message: impl Into<String>) -> Self {
        Self::new(Code::StateConflict, message)
    }

    pub fn dependency(message: impl Into<String>) -> Self {
        Self::new(Code::DependencyFailed, message)
    }

    /// 5xxx 兜底：任何未映射的内部错误转 ServiceUnavailable，原因进日志。
    pub fn internal(cause: anyhow::Error) -> Self {
        Self::new(Code::ServiceUnavailable, "服务暂时不可用，请稍后重试").with_cause(cause)
    }

    pub fn with_field(mut self, field: impl Into<String>, rule: impl Into<String>) -> Self {
        self.fields.push(FieldErr {
            field: field.into(),
            rule: rule.into(),
        });
        self
    }

    pub fn with_cause(mut self, cause: anyhow::Error) -> Self {
        self.cause = Some(cause);
        self
    }
}

/// 便捷转换：anyhow 错误自动映射 5xxx（`?` 直接传播场景）。
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_values_are_frozen() {
        // 契约红线：数值一经发布不得变更
        assert_eq!(Code::Unauthorized as i64, 1001);
        assert_eq!(Code::StateConflict as i64, 2000);
        assert_eq!(Code::Conflict as i64, 2009);
        assert_eq!(Code::ServiceUnavailable as i64, 5003);
    }

    #[test]
    fn http_status_mapping() {
        assert_eq!(
            Code::Unauthorized.http_status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            Code::AccountLocked.http_status(),
            axum::http::StatusCode::LOCKED
        );
        assert_eq!(
            Code::Conflict.http_status(),
            axum::http::StatusCode::CONFLICT
        );
    }
}
