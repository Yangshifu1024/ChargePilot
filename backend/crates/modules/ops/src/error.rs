//! ops 域错误类型（蓝图第五章：域内错误词表集中收口）。

/// 业务错误：映射码值与文案沿用。
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    /// 凭据错误或登录已失效（401/1001）。
    #[error("credentials invalid")]
    Credentials,
    /// 账号锁定（423/2006，文案透出锁定时长）。
    #[error("{0}")]
    Locked(String),
    /// TOTP 验证码已被消费（重放，401/1001）。
    #[error("该验证码已被使用，请等待下一个验证码")]
    UsedTotp,
    /// TOTP 无效或过期（401/1001）。
    #[error("invalid totp")]
    InvalidTotp,
    /// MFA 挑战失效（401/1001）。
    #[error("mfa challenge invalid")]
    InvalidChallenge,
    /// 登录频率超限（429/4291，Retry-After 60）。
    #[error("登录尝试过于频繁，请稍后再试")]
    LoginRate,
    /// 权限不足（403/1003）。
    #[error("没有操作权限")]
    Permission,
    /// 存储不可用（503/5003）。
    #[error("storage failure: {0}")]
    Storage(#[from] anyhow::Error),
    /// 业务输入错误（400/1005，文案透出）。
    #[error("{0}")]
    Input(String),
    /// 业务冲突（409/2009，文案透出）。
    #[error("{0}")]
    Conflict(String),
    /// 资源不存在（404/1004，文案透出）。
    #[error("{0}")]
    NotFound(String),
}
impl OpsError {
    pub fn bad_input(msg: impl Into<String>) -> Self {
        OpsError::Input(msg.into())
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        OpsError::Conflict(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        OpsError::NotFound(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        let _ = msg;
        OpsError::Permission
    }
}
impl From<sqlx::Error> for OpsError {
    fn from(e: sqlx::Error) -> Self {
        OpsError::Storage(e.into())
    }
}
impl From<redis::RedisError> for OpsError {
    fn from(e: redis::RedisError) -> Self {
        OpsError::Storage(e.into())
    }
}

/// MFA 业务错误。
#[derive(Debug, thiserror::Error)]
pub enum MfaError {
    /// 输入不合法（400/1005）。
    #[error("请按操作提供当前密码、绑定标识和六位验证码")]
    Input,
    /// 带文案的输入错误（400/1005，管理端 confirm 用）。
    #[error("{0}")]
    InputMsg(String),
    /// 密码或验证码校验失败（400/1002）。
    #[error("密码或验证码校验失败")]
    Proof,
    /// 验证码已被消费。
    #[error("该验证码已使用，请等待下一个验证码")]
    UsedTotp,
    /// 待绑定记录已过期或被替换（409/2009）。
    #[error("绑定已过期或已被替换，请重新开始")]
    Pending,
    /// 当前状态不支持该动作（409/2009）。
    #[error("双因素认证状态已改变，请刷新后重试")]
    State,
    /// 同账号设置租锁被占用（409/2009）。
    #[error("账号安全设置正在处理中，请稍后重试")]
    Busy,
    /// 安全验证过于频繁（429/4291 + Retry-After 60）。
    #[error("安全验证过于频繁，请稍后重试")]
    Rate,
    /// 凭据失效（401/1001）。
    #[error("credentials invalid")]
    Credentials,
    /// 本人 MFA 只走个人安全设置（409/2009）。
    #[error("请使用本人账号的安全设置修改双因素认证")]
    SelfMfa,
    /// 记录不存在或已删除（404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储错误（503/5003）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}
impl From<sqlx::Error> for MfaError {
    fn from(e: sqlx::Error) -> Self {
        MfaError::Storage(anyhow::anyhow!(e.to_string()))
    }
}
impl From<redis::RedisError> for MfaError {
    fn from(e: redis::RedisError) -> Self {
        MfaError::Storage(anyhow::anyhow!(e.to_string()))
    }
}
