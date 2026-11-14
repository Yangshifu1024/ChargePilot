//! 业务域模块：identity（C 端身份：登录、会话、手机号）。
//!
//! 语义对齐旧版：登录即建户（与零余额钱包同事务）、development 换购器（`dev:` 前缀
//! 命名空间）、会话 7 天绝对寿命 + Lua 轮换保留 32 旧摘要、访问令牌 JWT（user kind）。

pub mod admin_query;
pub mod phone;
pub mod repo;
pub mod service;
pub mod session;
pub mod wechat;

/// 用户聚合（`user` 表行投影）。
#[derive(Debug, Clone)]
pub struct User {
    pub id: u64,
    pub openid: String,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    pub phone: Option<String>,
    pub status: chargepilot_domain::identity::UserStatus,
}

impl User {
    pub fn is_active(&self) -> bool {
        self.status == chargepilot_domain::identity::UserStatus::Active
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
