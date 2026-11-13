//! ChargePilot 单体服务入口：装配与进程生命周期（`serve`）+ 数据库迁移（`migrate`）。
//!
//! 停机顺序（旧版规则沿用）：HTTP 停止接活 → 后台任务取消并 join → 最后关连接池。

use anyhow::Context as _;
use clap::{Parser, Subcommand};

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

/// 阶段 01 装配：连接池 + 事件投递循环 + 健康/指标 HTTP + 优雅停机。
/// 业务路由（阶段 02 起）与设备 TCP（阶段 06）在此文件扩展。
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

    // 事件投递循环（处理器登记表随各业务域落地；kind 无处理器时退避不崩溃）
    let registry = std::sync::Arc::new(chargepilot_events::runtime::Registry::new());
    tasks.spawn("event-dispatcher", {
        chargepilot_events::runtime::spawn_dispatch_loop(db.clone(), registry, token.clone())
    });

    // HTTP：/health/live /health/ready /metrics（业务路由阶段 02 起）
    let readiness = Readiness {
        db: db.clone(),
        redis: redis_pool.clone(),
    };
    let app = axum::Router::new()
        .route("/health/live", axum::routing::get(|| async { "ok" }))
        .route("/health/ready", axum::routing::get(readiness_check))
        .route(
            "/metrics",
            axum::routing::get(move || {
                let handle = prometheus.clone();
                async move { handle.render() }
            }),
        )
        .with_state(readiness)
        .fallback(not_found);
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

#[derive(Clone)]
struct Readiness {
    db: sqlx::MySqlPool,
    redis: chargepilot_platform::redisx::Pool,
}

async fn readiness_check(
    axum::extract::State(state): axum::extract::State<Readiness>,
) -> &'static str {
    let db_ok = sqlx::query("SELECT 1").execute(&state.db).await.is_ok();
    let redis_ok = match state.redis.get().await {
        Ok(mut conn) => redis::cmd("PING")
            .query_async::<()>(&mut conn)
            .await
            .is_ok(),
        Err(_) => false,
    };
    if db_ok && redis_ok { "ok" } else { "degraded" }
}

async fn not_found(headers: axum::http::HeaderMap) -> axum::response::Response {
    let rid = chargepilot_platform::httpx::request_id_from(&headers);
    chargepilot_platform::httpx::not_found_route(&rid)
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
