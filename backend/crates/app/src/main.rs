//! ChargePilot 单体服务入口。
//!
//! 本进程做两件事：装配并跑起来（`serve`），以及单独升级数据库（`migrate`）。
//! 停机顺序固定为：先让 HTTP 不再接新请求 → 再取消并等待后台任务结束 → 最后关闭连接池。

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::sync::Arc;

mod simulator_batch;

#[derive(Parser)]
#[command(
    name = "chargepilot",
    version,
    about = "充电桩运营平台系统"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动完整服务：HTTP API、设备 TCP 接入、后台任务
    Serve,
    /// 只跑数据库迁移，升到最新 schema，不启动业务服务
    Migrate,
    /// 批量虚拟设备压测（非正式服务路径，给容量验收用）
    SimulatorBatch(simulator_batch::BatchArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志级别优先读 RUST_LOG；没配则默认 info
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Cmd::Serve => serve().await,
        Cmd::Migrate => migrate().await,
        Cmd::SimulatorBatch(args) => simulator_batch::run(args).await,
    }
}

/// 运维子命令：按 `DATABASE_URL` 打开库、执行迁移、打印本次应用了几个版本后退出。
///
/// 故意不走全量配置加载——迁移场景只需要库地址。
async fn migrate() -> anyhow::Result<()> {
    // 直读环境变量是有意为之：迁移/压测子命令不加载整套凭据配置
    #[allow(clippy::disallowed_methods)]
    let url = std::env::var("DATABASE_URL").context("缺少 DATABASE_URL 环境变量")?;
    let pool = chargepilot_storage::open_pool(&url, 2)
        .await
        .context("打开数据库连接池失败")?;
    let applied = chargepilot_storage::migrator::run_migrations(&pool).await?;
    println!("迁移完成：本次应用 {} 个版本", applied.len());
    pool.close().await;
    Ok(())
}

/// 正式服务启动入口：把库、Redis、号段、设备网关、事件循环、计费/退款、
/// HTTP 路由全部装好，然后阻塞到收到停机信号并按序退出。
async fn serve() -> anyhow::Result<()> {
    let cfg = chargepilot_platform::config::Config::from_env()?;
    let prometheus = chargepilot_platform::metrics::install()?;

    let db = chargepilot_storage::open_pool(&cfg.database_url, 32).await?;
    // 启动时自动迁移一次（幂等）。需要「只升级库、不启服务」时用 `migrate` 子命令。
    let applied = chargepilot_storage::migrator::run_migrations(&db).await?;
    if !applied.is_empty() {
        tracing::info!(count = applied.len(), "启动迁移完成");
    }
    let redis_pool = chargepilot_platform::redisx::open_pool(&cfg.redis_url, 16)?;
    // 号段分配器：批量预取 ID，避免每次写库都抢一次号
    let allocator = chargepilot_platform::ids::SegmentAllocator::new(db.clone(), 500);
    allocator.warm_up().await.context("号段分配器预热失败")?;
    tracing::info!("号段分配器就绪");

    let mut tasks = chargepilot_platform::lifecycle::TaskManager::new();
    let token = tasks.token();
    let allocator = Arc::new(allocator);

    // —— 设备接入：在线快照、遥测缓冲、会话表、TCP 监听 ——
    let snapshots = Arc::new(chargepilot_gateway_runtime::SnapshotStore::new());
    let telemetry = Arc::new(chargepilot_gateway_runtime::TelemetryBuffer::new());
    // GatewaySink：设备上报事件的统一落点（改快照、写审计、塞遥测缓冲）
    let gateway_sink = Arc::new(chargepilot_gateway_runtime::GatewaySink::new(
        db.clone(),
        snapshots.clone(),
        telemetry.clone(),
    ));
    let session_registry = Arc::new(chargepilot_gateway_runtime::SessionRegistry::new());
    // 遥测不逐条写库：后台约每 200ms 冲刷一批；停机时先冲净再关库
    let (telemetry_stop, telemetry_stop_rx) = tokio::sync::watch::channel(false);
    tasks.spawn("telemetry-flush", {
        let telemetry = telemetry.clone();
        let db = db.clone();
        let stop_rx = telemetry_stop_rx;
        async move {
            chargepilot_gateway_runtime::telemetry::run_flush_loop(db, telemetry, stop_rx).await;
            Ok(())
        }
    });
    // 设备 TCP 监听地址来自配置（默认 0.0.0.0:9100）
    let device_addr: std::net::SocketAddr = cfg
        .listen
        .device_addr
        .parse()
        .context("DEVICE_ADDR 解析失败")?;
    tasks.spawn("device-listener", {
        let gateway_sink = gateway_sink.clone();
        let session_registry = session_registry.clone();
        let token = token.clone();
        async move {
            // 协议适配器 + 同一 sink 既收事件又发下行；会话表按连接跟踪在线设备
            chargepilot_gateway_runtime::DeviceListener::new(
                Arc::new(chargepilot_protocol_dc589::adapter::TcpAdapter::new()),
                gateway_sink.clone(),
                gateway_sink,
                session_registry,
            )
            .run(device_addr, token)
            .await
        }
    });

    // —— 事件投递：支付确认后真正下发启动；设备回执/结束事件推进订单 ——
    let mut registry = chargepilot_events::runtime::Registry::new();
    // 用户付完款 → 经网关命令通道给桩下发启动（桩侧应答异步回来后再推进状态）
    registry.register(
        chargepilot_events::EventKind::ChargePaymentConfirmed,
        Arc::new(chargepilot_mod_charging::StartDispatcher {
            pool: db.clone(),
            allocator: (*allocator).clone(),
            sink: Arc::new(chargepilot_gateway_runtime::GatewayCommandSink::new(
                session_registry.clone(),
            )),
            simulation: false,
        }),
    );
    // 设备侧事件（启动应答、结束上报等）→ 更新订单/回执
    registry.register(
        chargepilot_events::EventKind::DeviceEvent,
        Arc::new(chargepilot_gateway_runtime::DeviceEventDispatcher {
            pool: db.clone(),
            allocator: (*allocator).clone(),
            registry: session_registry.clone(),
        }),
    );
    let registry = Arc::new(registry);
    tasks.spawn("event-dispatcher", {
        chargepilot_events::runtime::spawn_dispatch_loop(db.clone(), registry, token.clone())
    });

    // —— 计费与退款：各 15 秒扫一轮；处理器本身幂等，多跑无害 ——
    tasks.spawn("billing-loop", {
        let db = db.clone();
        let token = token.clone();
        async move {
            loop {
                if token.is_cancelled() {
                    return Ok(());
                }
                // 领取到期计费任务，逐单结算；单笔失败只打日志，不拖死整轮
                match chargepilot_mod_settlement::billing::due_jobs(&db, 50).await {
                    Ok(ids) => {
                        for id in ids {
                            if let Err(e) =
                                chargepilot_mod_settlement::billing::process_job(&db, id).await
                            {
                                tracing::warn!(charge_order_id = id, error = %e, "计费任务处理失败");
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "计费任务领取失败"),
                }
                token.sleep_or_cancelled(std::time::Duration::from_secs(15)).await;
            }
        }
    });
    tasks.spawn("refund-executor", {
        let db = db.clone();
        let token = token.clone();
        async move {
            loop {
                if token.is_cancelled() {
                    return Ok(());
                }
                // 每轮最多处理 50 笔待退款；失败下一轮再试
                if let Err(e) = chargepilot_mod_settlement::refund_executor::run_once(&db, 50).await
                {
                    tracing::warn!(error = %e, "退款执行失败");
                }
                token
                    .sleep_or_cancelled(std::time::Duration::from_secs(15))
                    .await;
            }
        }
    });

    // —— 兜底扫描：约 30 秒级（启动超时）与按小时（过期导出清理）——
    // 计费/退款仍走上面的独立快循环；这里只挂「慢一点也能接受」的兜底。
    let mut sweeps = chargepilot_platform::sweeperx::SweepRegistry::new(db.clone());
    let export_dir = Arc::new(cfg.export.dir.clone());
    sweeps.register(
        "charging-start-timeout",
        std::time::Duration::from_secs(30),
        {
            // 启动命令发出后长时间无应答 → 超时收口，避免订单卡在启动中
            move |pool| {
                Box::pin(
                    async move { chargepilot_mod_charging::sweeper::sweep_once(&pool, 5).await },
                )
            }
        },
    );
    sweeps.register("export-cleanup", std::time::Duration::from_secs(3600), {
        // 闭包会被多次调用：目录路径用 Arc 克隆进每次 future
        let export_dir = std::sync::Arc::clone(&export_dir);
        move |pool| {
            let dir = std::sync::Arc::clone(&export_dir);
            Box::pin(async move {
                chargepilot_mod_export::cleanup_expired(&pool, &dir)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            })
        }
    });
    sweeps.spawn_all(&mut tasks, token.clone());

    // 若配置了引导账号密码，确保库里至少有一个可用的后台管理员
    let bootstrap_user = cfg.bootstrap.user.clone();
    let bootstrap_pass = cfg.bootstrap.password.clone();
    if !bootstrap_user.is_empty() {
        chargepilot_mod_ops::repo::ensure_bootstrap(
            &db,
            &allocator,
            &bootstrap_user,
            &bootstrap_pass,
        )
        .await
        .context("后台引导账号创建失败")?;
    }

    // —— HTTP：支付/钱包/登录/停充等依赖一次性装进 ApiDeps，再挂路由 ——
    let payment_services = chargepilot_http_api::state::PaymentServices::assemble(
        db.clone(),
        allocator.clone(),
        cfg.payment_mode,
        &cfg.wechat,
    )
    .context("支付域装配失败")?;
    let deps = Arc::new(chargepilot_http_api::state::ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(cfg.redis_url.clone()),
        jwt_secret: Arc::new(cfg.auth.jwt_secret.clone()),
        allocator,
        exchanger: Arc::new(chargepilot_http_api::state::exchanger_of(
            cfg.login_mode,
            &cfg.wechat,
        )),
        wallet: Some(Arc::new(
            chargepilot_http_api::state::WalletServices::build(db.clone()),
        )),
        checkout: Some(payment_services.checkout),
        wallet_recharge: Some(payment_services.wallet_recharge),
        simulation_payment: payment_services.simulation_payment,
        export: chargepilot_mod_export::ExportConfig {
            export_dir: cfg.export.dir.clone(),
            max_rows: cfg.export.max_rows,
        },
        // 用户点停止 → 经命令通道通知在线桩
        charging_stop: Some(Arc::new(chargepilot_mod_charging::StopService {
            pool: db.clone(),
            sink: Arc::new(chargepilot_gateway_runtime::GatewayCommandSink::new(
                session_registry.clone(),
            )),
        })),
        snapshots: Some(snapshots.clone()),
    });
    let app = chargepilot_http_api::build_router(deps, prometheus).await;
    let listener = tokio::net::TcpListener::bind(&cfg.listen.http_addr).await?;
    tracing::info!(addr = %cfg.listen.http_addr, "HTTP 服务启动");

    // —— 停机：信号 → HTTP 优雅退出 → 冲净遥测 → join 后台任务 → 关 Redis/DB ——
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_stop_signal().await;
        tracing::info!("收到停止信号，HTTP 停止接活");
        let _ = stop_tx.send(true);
    });
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let mut rx = stop_rx;
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
    let server_task = tokio::spawn(async move { server.await.context("HTTP 服务异常退出") });
    let _ = server_task.await;

    let _ = token; // 取消信号由 TaskManager::shutdown 统一广播
    // 遥测冲刷用独立开关：先把缓冲写完，再关连接池，避免丢末批数据
    let _ = telemetry_stop.send(true);
    let stuck = tasks.shutdown(std::time::Duration::from_secs(15)).await;
    if !stuck.is_empty() {
        tracing::warn!(?stuck, "部分任务停机超时");
    }
    redis_pool.close();
    db.close().await;
    tracing::info!("进程退出完成");
    Ok(())
}

/// 等进程停机信号：SIGTERM（容器/systemd 常见）或 SIGINT（Ctrl-C）。
async fn wait_stop_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => panic!("注册 SIGTERM 失败: {e}"),
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => panic!("注册 SIGINT 失败: {e}"),
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
