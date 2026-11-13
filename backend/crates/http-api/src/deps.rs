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
    /// 钱包域服务组（批 2）：退款/风控共用一个 RefundService 实例。
    pub wallet: Option<Arc<WalletServices>>,
}

/// 钱包域装配（payment 来源读取 + settlement 退款记录 + wallet 服务）。
#[derive(Clone)]
pub struct WalletServices {
    pub refund: Arc<chargepilot_mod_wallet::refund::RefundService>,
    pub risk: Arc<chargepilot_mod_wallet::risk::RiskService>,
}

impl WalletServices {
    /// 按默认归属装配（payment 提供 sources、settlement 提供 recorder）。
    pub fn build(db: sqlx::MySqlPool) -> Self {
        let refund = Arc::new(chargepilot_mod_wallet::refund::RefundService {
            db: db.clone(),
            sources: Arc::new(chargepilot_mod_payment::sources::RechargeSources),
            recorder: Arc::new(chargepilot_mod_settlement::recorder::RefundRecorder),
        });
        let risk = Arc::new(chargepilot_mod_wallet::risk::RiskService {
            db,
            refund: refund.clone(),
        });
        Self { refund, risk }
    }
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
