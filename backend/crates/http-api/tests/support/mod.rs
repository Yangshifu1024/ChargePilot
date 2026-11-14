#![allow(dead_code)] // 各测试二进制只用到支撑全集的子集

//! 共享集成测试支撑（票 04）：构建器装配、跨文件互斥锁、请求与登录辅助。
//!
//! **形态对齐票 17**：`TestAppBuilder` 的各域开关方法（`with_wallet` 等）
//! 对应票 17 目标形态「按域分组的最小句柄组」——票 17 落地 `AppState`
//! 时，本模块仅替换 `build` 内部的装配实现（可选字段清零、FromRef 子状态），
//! 构建器 API 与各测试文件的开关组合保持不变，测试文件不二次改动。
//!
//! **并发互斥与种子隔离边界**：全部 flow 测试共享一个开发库（无独立
//! schema），种子数据（用户/订单/卡号）一律以 `rand_suffix()` 派生的随机
//! 后缀命名，唯一键互不冲突；状态断言仅针对本测试创建的种子行。跨文件
//! 唯一的全局可变资源是号段分配器缓存与 Redis 登录频次键：前者按段
//! 领取天然互斥；后者的登录限流按来源 IP 计数，`req()` 每请求派生随机
//! `x-real-ip`（`10.x.x.x` 段），重复运行与并行文件间互不触发 4291。
//! 限流行为自身的断言（admin_auth_flow）用专用固定 IP 走 `req_with_ip`。
//!
//! **锁语义**：统一跨进程端口锁（`flow_lock`，绑定 `127.0.0.1:39701`
//! 等待获取）。替代旧进程内 `Mutex<()>`——后者在 `cargo test` 下不互斥
//! （每个测试文件是独立测试二进制，Mutex 各自独立），端口锁对同一工作机
//! 的并发 `cargo test` 与 `tasks.mjs` 两级调用都生效。
//!
//! **跳过与失败可区分**：`build()` 在环境变量缺失或连接失败时直接
//! `panic!`——集成轮（`--include-ignored`）中一律判失败，不静默跳过；
//! 日常无环境变量轮由 `#[ignore]` 标记语义承担（不进入默认 `cargo test`）。

use chargepilot_http_api::state::{ApiDeps, WalletServices};
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::Value;
use std::sync::Arc;

/// 已装配的测试应用：路由器与句柄的所有权一次交出。
pub struct TestApp {
    pub router: axum::Router,
    pub db: Arc<sqlx::MySqlPool>,
    pub allocator: Arc<SegmentAllocator>,
}

impl TestApp {
    /// 构建器入口。默认最小装配：仅身份域必需句柄（库/Redis/JWT/号段/
    /// 微信开发交换器），无任何业务域服务。
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder {
            wallet: false,
            payment: false,
            simulation_route: false,
            charging_stop: false,
            snapshots: false,
            exchanger: None,
            export: None,
        }
    }
}

/// 构建器：按需开启各域句柄组（票 17 的按域形态预知——开关语义在
/// AppState 化后保持）。
pub struct TestAppBuilder {
    wallet: bool,
    payment: bool,
    /// 仅开模拟确认路由（charging flow：支付模拟确认不触达 checkout 域）。
    simulation_route: bool,
    charging_stop: bool,
    snapshots: bool,
    /// 微信交换器覆写（Wechat 分支测试注入；默认 Development）。
    exchanger: Option<chargepilot_mod_identity::wechat::Exchanger>,
    /// 导出配置（export flow 注入目录；缺省空目录走 service 层临时目录回退）。
    export: Option<chargepilot_mod_export::ExportConfig>,
}

impl TestAppBuilder {
    /// 钱包域句柄组（退款/风控/充值读取，`WalletServices::build` 默认归属）。
    #[must_use]
    pub fn with_wallet(mut self) -> Self {
        self.wallet = true;
        self
    }

    /// 支付域句柄组（simulation 语义，旧版 PAYMENT_MODE=simulation）：
    /// CheckoutService + 钱包充值桥 + 模拟确认路由一并开启。
    #[must_use]
    pub fn with_simulation_payment(mut self) -> Self {
        self.payment = true;
        self
    }

    /// 仅开模拟确认路由（`simulation_payment` 布尔位；充电流程的模拟
    /// 支付确认不触达 checkout 域——与 charging flow 现状一致）。
    #[must_use]
    pub fn with_simulation_route(mut self) -> Self {
        self.simulation_route = true;
        self
    }

    /// 充电停止服务（模拟命令下沉 `SimulationSink`）。
    #[must_use]
    pub fn with_charging_stop(mut self) -> Self {
        self.charging_stop = true;
        self
    }

    /// 设备实时快照存储（内存态，缺省回退查库）。
    #[must_use]
    pub fn with_snapshots(mut self) -> Self {
        self.snapshots = true;
        self
    }

    /// 微信交换器覆写（user_identity_cards 的 Wechat 分支测试）。
    #[must_use]
    pub fn with_exchanger(
        mut self,
        exchanger: chargepilot_mod_identity::wechat::Exchanger,
    ) -> Self {
        self.exchanger = Some(exchanger);
        self
    }

    /// 导出配置（export flow 注入目录，经 ApiDeps 与处理器同源）。
    #[must_use]
    pub fn with_export(mut self, dir: impl Into<String>, max_rows: i64) -> Self {
        self.export = Some(chargepilot_mod_export::ExportConfig {
            export_dir: dir.into(),
            max_rows,
        });
        self
    }

    /// 装配并返回测试应用。任何一步失败（环境缺失、连接失败、预热失败）
    /// 都 `panic!`——集成轮判失败，不静默跳过。
    #[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
    pub async fn build(self) -> TestApp {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("warn")
            .try_init();
        let db_url = std::env::var("TEST_MYSQL_URL").unwrap_or_else(|_| {
            panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）")
        });
        let redis_url = std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| {
            panic!("集成测试需要 TEST_REDIS_URL（node tasks.mjs test-integration）")
        });
        let db = match chargepilot_storage::open_pool(&db_url, 8).await {
            Ok(d) => Arc::new(d),
            Err(e) => panic!("数据库连接失败：{e}"),
        };
        // 号段大小全文件统一 60：号段只影响分配批大小，测试断言以返回值
        // 比较 ID，与段宽无关（旧文件 20/50/70 的差异为复制残留）。
        let allocator = Arc::new(SegmentAllocator::new((*db).clone(), 60));
        allocator
            .warm_up()
            .await
            .unwrap_or_else(|e| panic!("号段预热失败：{e}"));

        // 支付域按 simulation 装配（旧版 PAYMENT_MODE=simulation 语义）。
        let (checkout, wallet_recharge) = if self.payment {
            let provider: Arc<dyn chargepilot_mod_payment::PayProvider> =
                Arc::new(chargepilot_mod_payment::channel::SimulationProvider);
            let scanner: Arc<dyn chargepilot_mod_payment::PortScanner> = Arc::new(
                chargepilot_mod_payment::scan::TablePortScanner::new((*db).clone()),
            );
            let checkout = Arc::new(chargepilot_mod_payment::CheckoutService {
                pool: (*db).clone(),
                allocator: (*allocator).clone(),
                scanner,
                provider: provider.clone(),
            });
            let bridge = Arc::new(
                chargepilot_mod_payment::recharge_bridge::RechargePaymentBridge {
                    pool: (*db).clone(),
                    allocator: (*allocator).clone(),
                    provider,
                },
            );
            let wallet_recharge = Arc::new(chargepilot_mod_wallet::recharge::RechargeService {
                db: (*db).clone(),
                payments: bridge,
            });
            (Some(checkout), Some(wallet_recharge))
        } else {
            (None, None)
        };

        let deps = Arc::new(ApiDeps {
            db: (*db).clone(),
            redis_url: Arc::new(redis_url),
            jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
            allocator: allocator.clone(),
            // 默认 Development（与全部测试文件现状一致；集成轮不注入
            // LOGIN_MODE）；Wechat 分支测试经 with_exchanger 覆写。
            exchanger: Arc::new(self.exchanger.unwrap_or_else(|| {
                chargepilot_http_api::state::exchanger_of(
                    chargepilot_platform::config::LoginMode::Development,
                    &Default::default(),
                )
            })),
            wallet: self
                .wallet
                .then(|| Arc::new(WalletServices::build((*db).clone()))),
            checkout,
            wallet_recharge,
            simulation_payment: self.payment || self.simulation_route,
            charging_stop: self.charging_stop.then(|| {
                Arc::new(chargepilot_mod_charging::StopService {
                    pool: (*db).clone(),
                    sink: Arc::new(chargepilot_mod_charging::SimulationSink),
                })
            }),
            snapshots: self
                .snapshots
                .then(|| Arc::new(chargepilot_gateway_runtime::SnapshotStore::new())),
            export: self.export.unwrap_or(chargepilot_mod_export::ExportConfig {
                export_dir: String::new(),
                max_rows: 0,
            }),
        });
        let router = chargepilot_http_api::build_router(deps, prometheus_handle()).await;
        TestApp {
            router,
            db,
            allocator,
        }
    }
}

/// 环境变量读取（panic 版）：种子直连库的文件用（不走 TestApp 装配）。
#[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
pub fn mysql_url() -> String {
    std::env::var("TEST_MYSQL_URL").unwrap_or_else(|_| {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）")
    })
}

/// 跨测试文件（跨测试二进制）互斥锁：绑定固定端口，持有者占锁、
/// 其余文件自旋等待。每测试主体的第一行 `let _flow_lock = flow_lock();`。
pub fn flow_lock() -> std::net::TcpListener {
    loop {
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:39701") {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 指标句柄单例（进程内唯一 recorder；重复 install 会 panic）。
pub fn prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect("prometheus recorder 安装失败")
        })
        .clone()
}

/// 种子随机后缀（8 字节 hex）：用户名/单号/卡号等唯一键的隔离来源。
pub fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn rand_ip() -> String {
    use rand::RngCore;
    let mut b = [0u8; 3];
    rand::rng().fill_bytes(&mut b);
    format!("10.{}.{}.{}", b[0], b[1], b[2])
}

/// 请求辅助（JSON 值形态，主流）：错误信封逐字节保留（`unwrap_or(Value::Null)`）。
/// 登录频次按来源 IP 计数——每请求随机 `x-real-ip`（见模块文档）。
pub async fn req(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (axum::http::StatusCode, Value) {
    let literal = body.as_ref().map(|v| v.to_string());
    req_str(router, method, path, literal.as_deref(), token).await
}

/// 请求辅助（字面量形态）：请求体逐字节发送（4 个旧文件的调用形态；
/// 序列化差异为零——不经 Value 往返）。
pub async fn req_str(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> (axum::http::StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-real-ip", rand_ip());
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned()))
            .expect("请求构造"),
        None => builder.body(Body::empty()).expect("请求构造"),
    };
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .expect("响应体");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 指定来源 IP 的请求（限流行为断言专用：固定 IP 触发/重置频次窗口）。
pub async fn req_with_ip(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    ip: &str,
) -> (axum::http::StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-real-ip", ip);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned()))
            .expect("请求构造"),
        None => builder.body(Body::empty()).expect("请求构造"),
    };
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
        .await
        .expect("响应体");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 管理端登录（断言风格）：失败即 panic（登录成功是绝大多数 flow 的前提）。
pub async fn login_admin(router: &axum::Router, username: &str, password: &str) -> String {
    let (status, body) = req(
        router,
        "POST",
        "/api/v1/admin/auth/login",
        Some(serde_json::json!({"username": username, "password": password})),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "登录失败: {body}");
    body["data"]["access_token"]
        .as_str()
        .expect("登录应返回 access_token")
        .to_string()
}

/// 管理端登录（Option 风格）：保留原「返回 None 由调用方断言」语义
/// （个别 flow 对登录失败路径有专属断言）。
pub async fn login_admin_opt(
    router: &axum::Router,
    username: &str,
    password: &str,
) -> Option<String> {
    let (status, body) = req(
        router,
        "POST",
        "/api/v1/admin/auth/login",
        Some(serde_json::json!({"username": username, "password": password})),
        None,
    )
    .await;
    if status != axum::http::StatusCode::OK {
        eprintln!("登录失败: {body}");
        return None;
    }
    body["data"]["access_token"].as_str().map(ToOwned::to_owned)
}

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
