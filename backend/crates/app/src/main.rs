//! ChargePilot 单体服务入口：装配与进程生命周期（`serve`）+ 数据库迁移（`migrate`）。
//!
//! 停机顺序（旧版规则沿用）：HTTP 停止接活 → 后台任务取消并 join → 最后关连接池。

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::sync::Arc;

mod simulator_batch;

#[derive(Parser)]
#[command(name = "chargepilot", about = "充电桩运营平台单体服务（Rust）")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动服务（HTTP + 设备 TCP + 后台任务）
    Serve,
    /// 把数据库迁移到最新版本
    Migrate,
    /// 批量模拟器（headless 压测加载源，非生产服务路径）
    SimulatorBatch(simulator_batch::BatchArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

async fn migrate() -> anyhow::Result<()> {
    // 运维子命令只需要库 DSN，不加载全量凭据配置；直读为显式登记的豁免点
    // （票 16：测试模块连接变量、migrate/压测子命令的数据库地址）。
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

/// 装配：连接池 + 号段 + 事件投递循环 + 业务路由 HTTP + 优雅停机。
/// 设备 TCP（阶段 06）在此文件扩展。
async fn serve() -> anyhow::Result<()> {
    let cfg = chargepilot_platform::config::Config::from_env()?;
    let prometheus = chargepilot_platform::metrics::install()?;

    let db = chargepilot_storage::open_pool(&cfg.database_url, 32).await?;
    // serve 自动先迁移（幂等；显式 `migrate` 子命令用于单独升级）。
    let applied = chargepilot_storage::migrator::run_migrations(&db).await?;
    if !applied.is_empty() {
        tracing::info!(count = applied.len(), "启动迁移完成");
    }
    let redis_pool = chargepilot_platform::redisx::open_pool(&cfg.redis_url, 16)?;
    let allocator = chargepilot_platform::ids::SegmentAllocator::new(db.clone(), 500);
    allocator.warm_up().await.context("号段分配器预热失败")?;
    tracing::info!("号段分配器就绪");

    let mut tasks = chargepilot_platform::lifecycle::TaskManager::new();
    let token = tasks.token();
    let allocator = Arc::new(allocator);

    // 设备接入装配：会话路由 + 事件 sink/审计 + 遥测缓冲（阶段 06 组件）。
    let snapshots = Arc::new(chargepilot_gateway_runtime::SnapshotStore::new());
    let telemetry = Arc::new(chargepilot_gateway_runtime::TelemetryBuffer::new());
    let gateway_sink = Arc::new(chargepilot_gateway_runtime::GatewaySink::new(
        db.clone(),
        snapshots.clone(),
        telemetry.clone(),
    ));
    let session_registry = Arc::new(chargepilot_gateway_runtime::SessionRegistry::new());
    // 遥测批量落库循环（200ms 节拍冲刷，停机信号后冲净退出）。
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
    // 设备 TCP 接入监听（DEVICE_ADDR，默认 0.0.0.0:9100）。
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

    // 事件投递循环：充电启动走真实设备命令通道（B8 回执异步推进）；
    // 设备事件（B8/BA/BB）由网关侧处理器推进回执。
    let mut registry = chargepilot_events::runtime::Registry::new();
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

    // 计费批次与退款执行器（各 15s 节拍；处理器幂等，重复调度无害）。
    tasks.spawn("billing-loop", {
        let db = db.clone();
        let token = token.clone();
        async move {
            loop {
                if token.is_cancelled() {
                    return Ok(());
                }
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

    // 兜底扫描统一注册（阶段 08）：统一节拍/滞后指标/内存态（诊断接口读）。
    // 计费批次与退款执行器保留独立循环（15s 快节拍），只把 30s 级兜底纳入框架。
    let mut sweeps = chargepilot_platform::sweeperx::SweepRegistry::new(db.clone());
    let export_dir = Arc::new(cfg.export.dir.clone());
    sweeps.register(
        "charging-start-timeout",
        std::time::Duration::from_secs(30),
        {
            move |pool| {
                Box::pin(
                    async move { chargepilot_mod_charging::sweeper::sweep_once(&pool, 5).await },
                )
            }
        },
    );
    sweeps.register("export-cleanup", std::time::Duration::from_secs(3600), {
        // Fn 闭包可多次调用：捕获经 Arc 克隆进每次调用的 future。
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

    // 后台引导账号（ADMIN_BOOTSTRAP_USER/PASSWORD 非空时确保存在）。
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

    // 业务路由装配（模式与凭据全部经统一配置，票 16）。
    let payment_services = chargepilot_http_api::deps::PaymentServices::assemble(
        db.clone(),
        allocator.clone(),
        cfg.payment_mode,
        &cfg.wechat,
    )
    .context("支付域装配失败")?;
    let deps = Arc::new(chargepilot_http_api::deps::ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(cfg.redis_url.clone()),
        jwt_secret: Arc::new(cfg.auth.jwt_secret.clone()),
        allocator,
        exchanger: Arc::new(chargepilot_http_api::deps::exchanger_of(
            cfg.login_mode,
            &cfg.wechat,
        )),
        wallet: Some(Arc::new(chargepilot_http_api::deps::WalletServices::build(
            db.clone(),
        ))),
        checkout: Some(payment_services.checkout),
        wallet_recharge: Some(payment_services.wallet_recharge),
        simulation_payment: payment_services.simulation_payment,
        export: chargepilot_mod_export::ExportConfig {
            export_dir: cfg.export.dir.clone(),
            max_rows: cfg.export.max_rows,
        },
        charging_stop: Some(Arc::new(chargepilot_mod_charging::StopService {
            pool: db.clone(),
            sink: Arc::new(chargepilot_gateway_runtime::GatewayCommandSink::new(
                session_registry.clone(),
            )),
        })),
        snapshots: Some(snapshots.clone()),
    });
    let app = chargepilot_http_api::build_router(
        deps,
        prometheus,
        cfg.static_assets.admin_dir.as_deref(),
        cfg.static_assets.h5_dir.as_deref(),
    )
    .await;
    let listener = tokio::net::TcpListener::bind(&cfg.listen.http_addr).await?;
    tracing::info!(addr = %cfg.listen.http_addr, "HTTP 服务启动");

    // 停机编排：信号 → HTTP 停止接活 → 取消后台任务并 join → 关连接池
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

    let _ = token; // 后台任务由 shutdown 统一广播取消
    // 遥测冲刷独立信号：先冲净缓冲再关连接池。
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
