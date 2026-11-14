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
    /// 导出配置（票 16 经统一配置传入，HTTP 层不再直读环境变量）。
    pub export: chargepilot_mod_export::ExportConfig,
    /// 充电停止服务（批 4）：授权与计费截止冻结；命令下沉随阶段 06 替换。
    pub charging_stop: Option<Arc<chargepilot_mod_charging::StopService>>,
    /// 设备实时快照（G2）：快照端点读实时端口读数，缺失回退查库；
    /// None 时不读内存快照，仅查遥测表。
    pub snapshots: Option<Arc<chargepilot_gateway_runtime::SnapshotStore>>,
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
    /// 按统一配置装配（票 16）：支付模式为枚举，凭据来自配置结构微信组。
    /// wechat_direct 要求微信凭据与 HTTP 执行器齐备（真实商户联调属上线前清单）。
    pub fn assemble(
        db: sqlx::MySqlPool,
        allocator: Arc<SegmentAllocator>,
        mode: chargepilot_platform::config::PaymentMode,
        wechat: &chargepilot_platform::config::WechatCreds,
    ) -> anyhow::Result<Self> {
        let provider: Arc<dyn chargepilot_mod_payment::PayProvider> = match mode {
            chargepilot_platform::config::PaymentMode::Simulation => {
                Arc::new(chargepilot_mod_payment::channel::SimulationProvider)
            }
            chargepilot_platform::config::PaymentMode::WechatDirect => {
                if wechat.appid.is_empty() {
                    anyhow::bail!("wechat_direct 需要 WECHAT_APPID");
                }
                if wechat.notify_url.is_empty() {
                    anyhow::bail!("wechat_direct 需要 WECHAT_NOTIFY_URL");
                }
                let config = chargepilot_mod_payment::wechat::WechatConfig::parse(
                    chargepilot_mod_payment::wechat::WechatCredsRaw {
                        mch_id: wechat.mch_id.clone(),
                        mch_serial_no: wechat.mch_serial.clone(),
                        key_pem: wechat.private_key_pem.clone(),
                        certs_raw: wechat.platform_certs.clone(),
                        api_v3: wechat.api_v3_key.clone(),
                    },
                )?;
                Arc::new(chargepilot_mod_payment::wechat::WechatDirectProvider {
                    config,
                    appid: wechat.appid.clone(),
                    http: Arc::new(chargepilot_mod_payment::wechat::WechatHttpExecutor),
                    notify_url: wechat.notify_url.clone(),
                })
            }
            chargepilot_platform::config::PaymentMode::Disabled => {
                Arc::new(chargepilot_mod_payment::channel::DisabledProvider)
            }
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
            simulation_payment: mode == chargepilot_platform::config::PaymentMode::Simulation,
        })
    }
}

/// 登录模式换出器（票 16：模式与凭据经统一配置传入，非法取值在配置层
/// 启动即失败，本函数只做枚举分派）。
pub fn exchanger_of(
    mode: chargepilot_platform::config::LoginMode,
    wechat: &chargepilot_platform::config::WechatCreds,
) -> Exchanger {
    match mode {
        chargepilot_platform::config::LoginMode::Development => Exchanger::Development,
        chargepilot_platform::config::LoginMode::Wechat => Exchanger::Wechat {
            appid: wechat.appid.clone(),
            secret: wechat.secret.clone(),
        },
    }
}
