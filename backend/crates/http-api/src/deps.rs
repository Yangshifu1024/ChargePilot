//! 全局路由依赖：各域服务共享的连接与配置。业务模块不持有这些——由本层装配。

use chargepilot_mod_identity::wechat::Exchanger;
use chargepilot_platform::ids::SegmentAllocator;
use std::sync::Arc;

#[derive(Clone)]
pub struct ApiDeps {
    pub db: sqlx::MySqlPool,
    pub redis_url: Arc<String>,
    pub jwt_secret: Arc<String>,
    pub allocator: Arc<SegmentAllocator>,
    pub exchanger: Arc<Exchanger>,
}

/// 登录模式解析（LOGIN_MODE 环境变量，旧版语义）。
pub fn exchanger_from_env() -> anyhow::Result<Exchanger> {
    match std::env::var("LOGIN_MODE")
        .unwrap_or_else(|_| "development".into())
        .as_str()
    {
        "development" => Ok(Exchanger::Development),
        "wechat" => Ok(Exchanger::Wechat {
            appid: std::env::var("WECHAT_APPID").unwrap_or_default(),
            secret: std::env::var("WECHAT_SECRET").unwrap_or_default(),
        }),
        other => anyhow::bail!("LOGIN_MODE 取值非法: {other}（可选 wechat / development）"),
    }
}
