//! ChargePilot 单体服务入口：装配与进程生命周期（`serve`）+ 数据库迁移（`migrate`）。
//! 子命令随阶段推进扩展（`seed` 对应旧版 devseed，随开发域落地）。

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
    /// 启动服务（HTTP 8080 + 设备 TCP 9100 + 后台任务）
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
        Cmd::Serve => {
            // 阶段 01 交付：配置加载、连接池、事件投递器与优雅停机框架的装配
            eprintln!("serve：骨架占位——能力在阶段 01 起逐步装配");
        }
        Cmd::Migrate => {
            let url = std::env::var("DATABASE_URL").context("缺少 DATABASE_URL 环境变量")?;
            let pool = chargepilot_storage::open_pool(&url, 2)
                .await
                .context("打开数据库连接池失败")?;
            let applied = chargepilot_storage::migrator::run_migrations(&pool).await?;
            println!("迁移完成：本次应用 {} 个版本", applied.len());
            pool.close().await;
        }
    }
    Ok(())
}
