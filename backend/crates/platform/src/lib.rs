//! 平台通用能力：无业务语义的横切组件。
//!
//! 模块：config（环境配置）/ error（错误码）/ httpx（信封与请求编号）/
//! ids（号段 ID）/ jwt / totp / password / sessiontoken / redisx / metrics / lifecycle。
//! 本 crate 不依赖任何业务模块；依赖方向由 workspace 强制。

pub mod config;
pub mod error;
pub mod httpx;
pub mod ids;
pub mod jwt;
pub mod lifecycle;
pub mod metrics;
pub mod password;
pub mod redisx;
pub mod sessiontoken;
pub mod sweeperx;
pub mod totp;
