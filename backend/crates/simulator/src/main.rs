//! ChargePilot 独立设备模拟器。
//!
//! 与生产服务分开构建：`single` 做单台指定板号联调，`batch` 做容量压测。
//! 复用 `protocol-dc589` 帧层，不自带任何服务侧装配。

use clap::{Parser, Subcommand};

mod batch;
mod single;

#[derive(Parser)]
#[command(
    name = "chargepilot-simulator",
    version,
    about = "ChargePilot 设备模拟器（单台联调 / 批量压测）"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 单连接模拟器：用指定板号接入一台虚拟设备
    Single(single::SingleArgs),
    /// 批量虚拟设备压测（容量验收加载源）
    Batch(batch::BatchArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::Single(args) => single::run(args).await,
        Cmd::Batch(args) => batch::run(args).await,
    }
}
