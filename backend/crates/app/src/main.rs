//! ChargePilot 单体服务入口：装配与进程生命周期（`serve`）+ 数据库迁移（`migrate`）。
//!
//! 停机顺序（旧版规则沿用）：HTTP 停止接活 → 后台任务取消并 join → 最后关连接池。

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use std::sync::Arc;

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
    }
}

async fn migrate() -> anyhow::Result<()> {
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
    let redis_pool = chargepilot_platform::redisx::open_pool(&cfg.redis_url, 16)?;
    let allocator = chargepilot_platform::ids::SegmentAllocator::new(db.clone(), 500);
    allocator.warm_up().await.context("号段分配器预热失败")?;
    tracing::info!("号段分配器就绪");

    let mut tasks = chargepilot_platform::lifecycle::TaskManager::new();
    let token = tasks.token();
    let allocator = Arc::new(allocator);

    // 事件投递循环：登记充电启动投递器（charge_payment_confirmed → 命令+预占）。
    let mut registry = chargepilot_events::runtime::Registry::new();
    registry.register(
        "charge_payment_confirmed",
        Arc::new(chargepilot_mod_charging::StartDispatcher {
            pool: db.clone(),
            allocator: (*allocator).clone(),
            sink: Arc::new(chargepilot_mod_charging::SimulationSink),
        }),
    );
    let registry = Arc::new(registry);
    tasks.spawn("event-dispatcher", {
        chargepilot_events::runtime::spawn_dispatch_loop(db.clone(), registry, token.clone())
    });

    // 启动超时兜底扫描（每 30s；超期 paid 订单 → failed + 退款登记）。
    tasks.spawn("charging-start-sweeper", {
        let db = db.clone();
        let token = token.clone();
        async move {
            loop {
                if token.is_cancelled() {
                    return Ok(());
                }
                if let Err(e) =
                    chargepilot_mod_charging::sweeper::sweep_once(&db, 5).await
                {
                    tracing::warn!(error = %e, "启动超时扫描失败");
                }
                token.sleep_or_cancelled(std::time::Duration::from_secs(30)).await;
            }
        }
    });

    // 后台引导账号（提供 ADMIN_BOOTSTRAP_USER/PASSWORD 时确保存在）
    let bootstrap_user = std::env::var("ADMIN_BOOTSTRAP_USER").unwrap_or_default();
    let bootstrap_pass = std::env::var("ADMIN_BOOTSTRAP_PASSWORD").unwrap_or_default();
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

    // 业务路由装配
    let payment_services =
        chargepilot_http_api::deps::PaymentServices::from_env(db.clone(), allocator.clone())
            .context("支付域装配失败")?;
    let deps = Arc::new(chargepilot_http_api::deps::ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(cfg.redis_url.clone()),
        jwt_secret: Arc::new(cfg.jwt_secret.clone()),
        allocator,
        exchanger: Arc::new(chargepilot_http_api::deps::exchanger_from_env()?),
        wallet: Some(Arc::new(chargepilot_http_api::deps::WalletServices::build(db.clone()))),
        checkout: Some(payment_services.checkout),
        wallet_recharge: Some(payment_services.wallet_recharge),
        simulation_payment: payment_services.simulation_payment,
        charging_stop: Some(Arc::new(chargepilot_mod_charging::StopService {
            pool: db.clone(),
            sink: Arc::new(chargepilot_mod_charging::SimulationSink),
        })),
    });
    let app = chargepilot_http_api::build_router(
        deps,
        prometheus,
        std::env::var("ADMIN_STATIC_DIR").ok().as_deref(),
    );
    let listener = tokio::net::TcpListener::bind(&cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "HTTP 服务启动");

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
    let mut term = signal(SignalKind::terminate()).expect("注册 SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("注册 SIGINT");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
