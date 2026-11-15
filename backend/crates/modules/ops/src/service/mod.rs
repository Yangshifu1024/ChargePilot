//! ops 域业务用例（蓝图第五章 service 层）：无 SQL。
#![allow(clippy::module_inception)] // service/service.rs 保持既有文件名

pub mod announcement;
pub mod management;
pub mod mfa;
pub mod service;
pub mod session;

// 既有外部路径保持：http-api 经 ops::service::{SessionService,...} 使用。
pub use service::{AccessReply, LoginOutcome, SessionService};
