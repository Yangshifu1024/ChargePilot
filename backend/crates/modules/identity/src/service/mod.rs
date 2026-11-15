//! identity 域业务用例（蓝图第五章 service 层）：无 SQL。
#![allow(clippy::module_inception)] // service/service.rs 保持既有文件名

pub mod phone;
pub mod service;
pub mod session;
pub mod wechat;

// 既有外部路径保持：http-api 经 service::SessionService 使用。
pub use service::SessionService;
