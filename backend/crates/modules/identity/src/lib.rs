//! identity 域：C 端身份（登录、会话、手机号）。
//! 跨域依赖（票 21 审计）：ops,wallet——后台账号实时权限（ops）；
//! 用户状态与钱包账户事实对账（wallet）。
//!
//! 语义：登录即建户（与零余额钱包同事务）、development 换购器（`dev:` 前缀
//! 命名空间）、会话 7 天绝对寿命 + Lua 轮换保留 32 旧摘要、访问令牌 JWT（user kind）。
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；`repo::` 顶级路径
//! 保持不变（marketing 等外部经 `mod_identity::repo::` 使用）。

pub use error::{AdminUserError, IdentityError, PhoneError};

pub mod error;
pub mod repo;
pub mod service;

pub use repo::user::{User, UserProfile};
// 既有外部路径保持：http-api 经这些顶级模块名使用。
pub use repo::admin_query;
pub use service::phone;
pub use service::service::SessionService;
pub use service::wechat;
