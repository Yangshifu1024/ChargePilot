//! identity 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 手机号业务错误（HTTP 映射沿用旧版字面量契约，见 http-api 适配层）。
#[derive(Debug, thiserror::Error)]
pub enum PhoneError {
    /// 正式路径须凭授权 code 且不得提交明文（400/1005）。
    #[error("phone authorization input invalid")]
    Authorization,
    /// 正式号码交换能力未装配（503/5003）。
    #[error("phone exchange unavailable")]
    Unavailable,
    /// 微信号码交换失败，不能采信客户端号码（400/1004）。
    #[error("phone exchange failed")]
    Exchange,
    /// 号码不符合中国大陆手机号约束（400/1005）。
    #[error("phone input invalid")]
    Input,
    /// 号码已被他人绑定（409/2009）。
    #[error("phone already taken")]
    Taken,
    /// 存储错误（503/5003）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for PhoneError {
    fn from(e: sqlx::Error) -> Self {
        // 并发同号最终由 uk_phone 唯一键仲裁（旧版 1062 → ErrPhoneTaken）。
        if let sqlx::Error::Database(db) = &e
            && db.code().as_deref() == Some("23000")
        {
            return PhoneError::Taken;
        }
        PhoneError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 后台用户查询错误（旧版 operations 哨兵与记录缺失的组合，HTTP 层翻译状态码）。
#[derive(Debug, thiserror::Error)]
pub enum AdminUserError {
    /// 实时权限不足（旧版 operations.ErrForbidden，403/1003）。
    #[error("当前账号无权执行此操作")]
    Forbidden,
    /// 用户不存在或已软删（旧版 gorm.ErrRecordNotFound，404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储不可用（503/5003）。
    #[error("存储不可用: {0}")]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for AdminUserError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 业务错误：HTTP 映射由 http-api 负责。
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// 刷新令牌格式错误、摘要不匹配或会话已过期（401/1001）。
    #[error("invalid or expired refresh token")]
    InvalidRefresh,
    /// 访问令牌无效（401/1001）。
    #[error("access token invalid")]
    TokenInvalid,
    /// JWT 有效但会话已被注销/过期（401/1001）。
    #[error("session expired")]
    SessionExpired,
    /// 账号冻结（403/1003）。
    #[error("account frozen")]
    AccountFrozen,
    /// 微信身份交换失败（502/3001）。
    #[error("wechat exchange failed: {0}")]
    ExchangeFailed(String),
    /// 存储不可用（503/5001）。
    #[error("storage failure: {0}")]
    Storage(#[from] anyhow::Error),
}
