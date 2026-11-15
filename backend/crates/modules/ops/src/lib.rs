//! 业务域模块：ops（后台账号、角色权限、审计、MFA、公告、仪表盘）。
//! 跨域依赖（票 21 审计）：无——无跨域依赖（依赖图根；仅依赖 domain/platform/storage）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 阶段 02 交付：后台登录（密码+MFA）、会话轮换、实时权限、审计与 bootstrap。
//! 账号/角色管理 CRUD、公告、仪表盘随第二批落地。

pub use error::{MfaError, OpsError};

pub mod announcement;
pub mod audit_query;
pub mod dashboard;
pub mod error;
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
