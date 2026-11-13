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
    /// 下单服务（批 3）：端口扫描、计费规则与渠道经本层装配。
    pub checkout: Option<Arc<chargepilot_mod_payment::CheckoutService>>,
    /// 钱包充值申请服务（批 3 接线：支付桥实现 RechargePayments）。
    pub wallet_recharge: Option<Arc<chargepilot_mod_wallet::recharge::RechargeService>>,
    /// 支付渠道模式：simulation 时开放模拟确认路由（旧版 PAYMENT_MODE）。
    pub simulation_payment: bool,
    /// 充电停止服务（批 4）：授权与计费截止冻结；命令下沉随阶段 06 替换。
    pub charging_stop: Option<Arc<chargepilot_mod_charging::StopService>>,
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

/// 支付域装配（渠道模式 + 下单服务 + 充值申请服务）。
pub struct PaymentServices {
    pub checkout: Arc<chargepilot_mod_payment::CheckoutService>,
    pub wallet_recharge: Arc<chargepilot_mod_wallet::recharge::RechargeService>,
    pub simulation_payment: bool,
}

impl PaymentServices {
    /// 按环境变量 PAYMENT_MODE 装配（disabled / simulation / wechat_direct）。
    /// wechat_direct 要求微信凭据与 HTTP 执行器齐备（真实商户联调属上线前清单）。
    pub fn from_env(db: sqlx::MySqlPool, allocator: Arc<SegmentAllocator>) -> anyhow::Result<Self> {
        let mode = std::env::var("PAYMENT_MODE").unwrap_or_else(|_| "simulation".into());
        let provider: Arc<dyn chargepilot_mod_payment::PayProvider> = match mode.as_str() {
            "simulation" => Arc::new(chargepilot_mod_payment::channel::SimulationProvider),
            "wechat_direct" => {
                let config = chargepilot_mod_payment::wechat::WechatConfig::from_env()?;
                let appid = std::env::var("WECHAT_APPID").unwrap_or_default();
                if appid.is_empty() {
                    anyhow::bail!("wechat_direct 需要 WECHAT_APPID");
                }
                let notify_url = std::env::var("WECHAT_NOTIFY_URL").unwrap_or_default();
                if notify_url.is_empty() {
                    anyhow::bail!("wechat_direct 需要 WECHAT_NOTIFY_URL");
                }
                Arc::new(chargepilot_mod_payment::wechat::WechatDirectProvider {
                    config,
                    appid,
                    http: Arc::new(chargepilot_mod_payment::wechat::WechatHttpExecutor),
                    notify_url,
                })
            }
            "disabled" => Arc::new(chargepilot_mod_payment::channel::DisabledProvider),
            other => anyhow::bail!("PAYMENT_MODE 取值非法: {other}"),
        };
        let scanner: Arc<dyn chargepilot_mod_payment::PortScanner> = Arc::new(
            chargepilot_mod_payment::scan::TablePortScanner::new(db.clone()),
        );
        let checkout = Arc::new(chargepilot_mod_payment::CheckoutService {
            pool: db.clone(),
            allocator: (*allocator).clone(),
            scanner,
            provider: provider.clone(),
        });
        let bridge = Arc::new(
            chargepilot_mod_payment::recharge_bridge::RechargePaymentBridge {
                pool: db.clone(),
                allocator: (*allocator).clone(),
                provider,
            },
        );
        let wallet_recharge = Arc::new(chargepilot_mod_wallet::recharge::RechargeService {
            db: db.clone(),
            payments: bridge,
        });
        Ok(Self {
            checkout,
            wallet_recharge,
            simulation_payment: mode == "simulation",
        })
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
