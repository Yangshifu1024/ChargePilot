//! identity 域表访问与读模型（蓝图第五章 repo 层）：SQL 只出现在本目录。
//! 既有 `mod_identity::repo::` 路径经下面的 re-export 保持不变。

pub mod admin_query;
pub mod phone;
pub mod user;

pub use user::{User, UserProfile, find_profile, is_active, lock_status_tx, login_or_create};
