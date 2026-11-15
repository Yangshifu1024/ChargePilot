//! 业务域模块：identity（C 端身份：登录、会话、手机号）。
//! 跨域依赖（票 21 审计）：ops,wallet——后台账号实时权限（ops）；用户状态与钱包账户事实对账（wallet）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//!
//! 语义对齐旧版：登录即建户（与零余额钱包同事务）、development 换购器（`dev:` 前缀
//! 命名空间）、会话 7 天绝对寿命 + Lua 轮换保留 32 旧摘要、访问令牌 JWT（user kind）。

pub use error::{AdminUserError, IdentityError, PhoneError};

pub mod admin_query;
pub mod error;
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
