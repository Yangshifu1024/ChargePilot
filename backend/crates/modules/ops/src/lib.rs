//! ops 域：后台账号、角色权限、审计、MFA、公告、仪表盘。
//! 跨域依赖（票 21 审计）：无——依赖图根（仅依赖 domain/platform/storage）。
//! 语义：后台登录（密码+MFA）、会话轮换、实时权限、审计与 bootstrap。
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；既有顶级模块路径
//! （repo/scope/audit_query/dashboard/announcement/management/mfa/service/session）
//! 全部经 re-export 保持。

pub use error::{MfaError, OpsError};

pub mod error;
pub mod repo;
pub mod service;

pub use repo::announcement::{AnnouncementRow, PublicAnnouncement};
pub use repo::{audit_query, dashboard, scope};
pub use service::{announcement, management, mfa, session};

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

/// 会话与登录挑战所需的最小身份（凭证材料不入会话）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountIdentity {
    pub id: u64,
    pub auth_version: u64,
    pub username: String,
}

/// 鉴权后管理员资料（JSON 键名与契约逐字节一致；auth_version 不出网）。
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
