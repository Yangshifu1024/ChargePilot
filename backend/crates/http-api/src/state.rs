//! 全局路由依赖：各域服务共享的连接与配置。业务模块不持有这些——由本层装配。

use axum::extract::FromRef;
use chargepilot_mod_identity::wechat::Exchanger;
use chargepilot_platform::ids::SegmentAllocator;
use std::sync::Arc;

// ── 按域最小句柄组：FromRef 从装配根派生，组内无可选字段。 ──
// 路由注册绑定装配开关（lib.rs build_router）：开关开启后该域路由才存在，
// 处理器对域服务的取用必达，不再有「服务未装配」的运行时降级分支。
// 会话基础件随组携带（用户端鉴权是横切，处理器单 State 提取自足）。

/// 用户端会话基础件：域组共用的鉴权句柄组。
#[derive(Clone)]
pub struct UserSessionBase {
    pub db: sqlx::MySqlPool,
    pub redis_url: Arc<String>,
    pub jwt_secret: Arc<String>,
    pub allocator: Arc<SegmentAllocator>,
    pub exchanger: Arc<Exchanger>,
}

impl FromRef<Arc<ApiDeps>> for UserSessionBase {
    fn from_ref(deps: &Arc<ApiDeps>) -> Self {
        Self {
            db: deps.db.clone(),
            redis_url: deps.redis_url.clone(),
            jwt_secret: deps.jwt_secret.clone(),
            allocator: deps.allocator.clone(),
            exchanger: deps.exchanger.clone(),
        }
    }
}

/// 钱包域句柄组（`wallet` 开关 → user_wallet 退款路由）。
#[derive(Clone)]
pub struct WalletState {
    pub base: UserSessionBase,
    pub wallet: Arc<WalletServices>,
}

#[allow(clippy::expect_used)] // 装配开关约束：路由注册已绑定开关，未装域路由不可达
impl FromRef<Arc<ApiDeps>> for WalletState {
    fn from_ref(deps: &Arc<ApiDeps>) -> Self {
        Self {
            base: UserSessionBase::from_ref(deps),
            wallet: deps
                .wallet
                .clone()
                .expect("钱包域未装配：路由未在装配开关打开时注册"), // 域服务必达契约
        }
    }
}

/// 支付域句柄组（`payment` 开关 → 下单/充值路由）。
#[derive(Clone)]
pub struct PaymentState {
    pub base: UserSessionBase,
    pub checkout: Arc<chargepilot_mod_payment::CheckoutService>,
    pub wallet_recharge: Arc<chargepilot_mod_wallet::recharge::RechargeService>,
}

#[allow(clippy::expect_used)] // 装配开关约束：路由注册已绑定开关，未装域路由不可达
impl FromRef<Arc<ApiDeps>> for PaymentState {
    fn from_ref(deps: &Arc<ApiDeps>) -> Self {
        Self {
            base: UserSessionBase::from_ref(deps),
            checkout: deps
                .checkout
                .clone()
                .expect("支付域未装配：路由未在装配开关打开时注册"),
            wallet_recharge: deps
                .wallet_recharge
                .clone()
                .expect("支付域未装配：路由未在装配开关打开时注册"),
        }
    }
}

/// 充电停止域句柄组（`charging_stop` 开关 → 停止路由）。
#[derive(Clone)]
pub struct ChargingStopState {
    pub base: UserSessionBase,
    pub stop: Arc<chargepilot_mod_charging::StopService>,
}

#[allow(clippy::expect_used)] // 装配开关约束：路由注册已绑定开关，未装域路由不可达
impl FromRef<Arc<ApiDeps>> for ChargingStopState {
    fn from_ref(deps: &Arc<ApiDeps>) -> Self {
        Self {
            base: UserSessionBase::from_ref(deps),
            stop: deps
                .charging_stop
                .clone()
                .expect("充电停止域未装配：路由未在装配开关打开时注册"),
        }
    }
}

#[derive(Clone)]
pub struct ApiDeps {
    pub db: sqlx::MySqlPool,
    pub redis_url: Arc<String>,
    pub jwt_secret: Arc<String>,
    pub allocator: Arc<SegmentAllocator>,
    pub exchanger: Arc<Exchanger>,
    /// 钱包域服务组：退款与风控共用一个 RefundService 实例。
    pub wallet: Option<Arc<WalletServices>>,
    /// 下单服务：端口扫描、计费规则与支付渠道在此装配。
    pub checkout: Option<Arc<chargepilot_mod_payment::CheckoutService>>,
    /// 钱包充值申请：支付桥实现 RechargePayments，接到充值服务。
    pub wallet_recharge: Option<Arc<chargepilot_mod_wallet::recharge::RechargeService>>,
    /// 支付渠道模式：simulation 时开放模拟确认路由。
    pub simulation_payment: bool,
    /// 导出配置。
    pub export: chargepilot_mod_export::ExportConfig,
    /// 充电停止：校验权限、冻结计费截止，经命令通道通知在线桩。
    pub charging_stop: Option<Arc<chargepilot_mod_charging::StopService>>,
    /// 设备实时快照：快照接口优先读内存端口读数，没有则回退查库；
    /// None 表示不走内存，只查遥测表。
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
    /// 按统一配置装配：支付模式为枚举，凭据来自配置结构微信组。
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

/// 按配置的登录模式构造 OAuth 换票器（凭证在启动阶段已校验，此处仅做枚举分派）。
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
