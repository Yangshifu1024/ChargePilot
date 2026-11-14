//! 业务域模块：ops（后台账号、角色权限、审计、MFA、公告、仪表盘）。
//! 阶段 02 交付：后台登录（密码+MFA）、会话轮换、实时权限、审计与 bootstrap。
//! 账号/角色管理 CRUD、公告、仪表盘随第二批落地。

pub mod announcement;
pub mod audit_query;
pub mod dashboard;
pub mod management;
pub mod mfa;
pub mod repo;
pub mod scope;
pub mod service;
pub mod session;

/// 后台账号行投影（不含口令散列等敏感列）。
#[derive(Debug, Clone)]
pub struct AdminAccount {
    pub id: u64,
    pub username: String,
    pub display_name: Option<String>,
    pub password_hash: String,
    pub role_id: Option<u64>,
    pub mfa_enabled: bool,
    pub mfa_secret: Option<String>,
    pub mfa_last_step: Option<i64>,
    pub status: chargepilot_domain::operations::AdminUserStatus,
    pub failed_login_count: u32,
    pub locked_until: Option<chrono::DateTime<chrono::Utc>>,
    pub auth_version: u64,
}

/// 会话与登录挑战所需的最小身份（旧版 AccountIdentity 语义：凭证材料不入会话）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountIdentity {
    pub id: u64,
    pub auth_version: u64,
    pub username: String,
}

/// 鉴权后管理员资料（JSON 键名与旧版逐字节一致；auth_version 不出网）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Profile {
    #[serde(skip)]
    pub auth_version: u64,
    #[serde(rename = "admin_user_id")]
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub role_name: String,
    #[serde(rename = "role_id")]
    pub role_id: u64,
    pub mfa_enabled: bool,
    /// 权限码列表按 code 排序；前端用它决定按钮可见性。
    pub permissions: Vec<String>,
}

/// 业务错误：映射码值与文案沿用旧版。
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
