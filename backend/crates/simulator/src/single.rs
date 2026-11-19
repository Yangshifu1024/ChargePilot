//! 单连接模拟器（`chargepilot-simulator single`）：一台指定板号的虚拟设备。
//!
//! 与批量模拟器共用帧层与会话动作，但面向手工联调：
//! - 板号由 `--device-id` 指定（DC589 固定 16 位十进制）；
//! - 默认空闲心跳，收到 B7 后转为充电遥测，收到 B9 回执并补 BB 结束帧；
//! - 断线自动重连；`--duration 0`（默认）时一直运行到 Ctrl-C；
//! - BB 结束帧回显 B7 携带的订单号，便于平台侧订单闭环。

use crate::batch::{
    Stats, expect_register_ok, scaled_interval, send_charge_end, send_command_result,
    send_heartbeat, write_frame,
};
use chargepilot_protocol_dc589::device::{self, DeviceIdentity};
use chargepilot_protocol_dc589::frame;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt as _, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::watch;

/// 单设备模拟器参数。
#[derive(Debug, Clone, clap::Args)]
pub struct SingleArgs {
    /// 设备主板编号（DC589 板号：固定 16 位十进制，如 5348000000000001）
    #[arg(long)]
    pub device_id: String,
    /// 网关设备接入地址（host:port，如 127.0.0.1:9100）
    #[arg(long)]
    pub target_addr: String,
    /// 运行秒数；0 表示一直运行到 Ctrl-C
    #[arg(long, default_value_t = 0)]
    pub duration: u64,
    /// 心跳/遥测加速倍率（1.0 = 真实节拍：空闲 60s、充电 15s）
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,
    /// 默认端口号；B7/B9 到达后以命令携带的端口为准
    #[arg(long, default_value_t = 1)]
    pub port: u8,
    /// 注册后立即上报充电遥测（默认空闲，等 B7 启动）
    #[arg(long)]
    pub charging: bool,
    /// 注册前把该设备写入 device_meta（幂等，已存在则不变）
    #[arg(long)]
    pub provision: bool,
    /// --provision 使用的数据库连接（缺省读 DATABASE_URL）
    #[arg(long)]
    pub database_url: Option<String>,
}

/// 由指定板号派生合法设备身份：模块号/SIM 取板号后 8 位，其余字段固定。
pub(crate) fn identity_for_board(board_id: &str) -> anyhow::Result<DeviceIdentity> {
    if board_id.len() != device::BOARD_ID_DIGITS || !board_id.bytes().all(|b| b.is_ascii_digit()) {
        anyhow::bail!(
            "--device-id 必须是 {} 位十进制板号（当前 {:?}）",
            device::BOARD_ID_DIGITS,
            board_id
        );
    }
    let tail = &board_id[board_id.len() - 8..];
    Ok(DeviceIdentity {
        board_id: board_id.to_string(),
        hardware_version: "SH10HP01".into(),
        software_id: "SH10SP02".into(),
        software_version: 107,
        module_id: format!("08631210{tail}"),
        sim: format!("898604041018{tail}"),
        signal: 20,
    })
}

/// 幂等预导入：device_meta 有行即允许注册，缺失则网关拒绝。
#[allow(clippy::disallowed_methods)] // 压测子命令：未传参时回退读取 DATABASE_URL
async fn provision(args: &SingleArgs) -> anyhow::Result<()> {
    let url = args
        .database_url
        .clone()
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .ok_or_else(|| anyhow::anyhow!("--provision 需要连接串：--database-url 或 DATABASE_URL"))?;
    let pool = chargepilot_storage::open_pool(&url, 2).await?;
    sqlx::query(
        "INSERT IGNORE INTO `device_meta` (`device_id`, `protocol_adapter`) \
         VALUES (?, 'dc589')",
    )
    .bind(&args.device_id)
    .execute(&pool)
    .await?;
    println!("预导入完成：{}（已存在则保持不变）", args.device_id);
    pool.close().await;
    Ok(())
}

/// 休眠或收到停机信号，返回 true 表示应停止。
async fn pause_or_stop(rx: &mut watch::Receiver<bool>, d: Duration) -> bool {
    if *rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = rx.changed() => true,
    }
}

fn print_stats(stats: &Stats) {
    println!("=== 单设备模拟器统计 ===\n{}", stats.snapshot());
}

/// 子命令入口：连接 → 注册 → 心跳/命令循环，断线重连直到到时或 Ctrl-C。
pub async fn run(args: SingleArgs) -> anyhow::Result<()> {
    if args.port == 0 || args.port > 64 {
        anyhow::bail!("--port 取值 1–64（当前 {}）", args.port);
    }
    let identity = identity_for_board(&args.device_id)?;
    if args.provision {
        provision(&args).await?;
    }

    println!(
        "单设备模拟器：device={} → {}（Ctrl-C 结束{}）",
        identity.board_id,
        args.target_addr,
        if args.duration > 0 {
            format!("，{} 秒后自动收尾", args.duration)
        } else {
            String::new()
        }
    );

    let stats = Arc::new(Stats::default());
    let idle_tick = scaled_interval(60, args.speedup);
    let charging_tick = scaled_interval(15, args.speedup);
    let deadline = (args.duration > 0).then(|| Instant::now() + Duration::from_secs(args.duration));
    let expired = || deadline.is_some_and(|d| Instant::now() >= d);
    // 到时分支放进 select，避免心跳间隔很长时越过 --duration 才退出。
    let deadline_future = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline_future);

    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = stop_tx.send(true);
        }
    });

    // 跨重连保持充电状态与累计量：与真实设备断线重连后的行为一致。
    let mut charging = args.charging;
    let mut active_order: Option<[u8; 8]> = None;
    let mut port = args.port;
    let mut charged_seconds = 0_u32;
    let mut charged_mwh = 0_u32;

    loop {
        if *stop_rx.borrow() || expired() {
            print_stats(&stats);
            return Ok(());
        }

        let stream = match TcpStream::connect(&args.target_addr).await {
            Ok(s) => s,
            Err(e) => {
                println!("连接 {} 失败：{e}；2 秒后重试", args.target_addr);
                if pause_or_stop(&mut stop_rx, Duration::from_secs(2)).await {
                    print_stats(&stats);
                    return Ok(());
                }
                continue;
            }
        };
        let (mut reader, writer) = stream.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(writer)));

        // 注册：全零会话号 A0，A1 带回服务端分配的会话号。
        let Ok(reg) = device::build_registration(&identity) else {
            anyhow::bail!("注册帧编码失败：板号 {}", identity.board_id);
        };
        let Ok(raw) = frame::encode(&reg) else {
            anyhow::bail!("注册帧序列化失败");
        };
        if write_frame(&writer, &raw).await.is_err() {
            stats.conn_errors.fetch_add(1, Ordering::Relaxed);
            if pause_or_stop(&mut stop_rx, Duration::from_secs(2)).await {
                print_stats(&stats);
                return Ok(());
            }
            continue;
        }
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);

        let Some(session) = expect_register_ok(&mut reader, &stats, Duration::from_secs(10)).await
        else {
            println!(
                "注册失败或被拒绝：确认 device_meta 含 {}（可加 --provision），3 秒后重试",
                identity.board_id
            );
            if pause_or_stop(&mut stop_rx, Duration::from_secs(3)).await {
                print_stats(&stats);
                return Ok(());
            }
            continue;
        };
        stats.registrations_ok.fetch_add(1, Ordering::Relaxed);
        println!(
            "注册成功：device={} session={:02x?}",
            identity.board_id, session
        );

        // 会话主循环：心跳节拍与下行命令竞争。
        let mut alive = true;
        while alive {
            if *stop_rx.borrow() || expired() {
                finish_charge(
                    &writer,
                    session,
                    port,
                    active_order,
                    charging,
                    charged_seconds,
                    charged_mwh,
                    &stats,
                )
                .await;
                print_stats(&stats);
                return Ok(());
            }
            let tick = if charging { charging_tick } else { idle_tick };
            tokio::select! {
                _ = &mut deadline_future => {
                    finish_charge(
                        &writer,
                        session,
                        port,
                        active_order,
                        charging,
                        charged_seconds,
                        charged_mwh,
                        &stats,
                    )
                    .await;
                    print_stats(&stats);
                    return Ok(());
                }
                _ = stop_rx.changed() => {
                    finish_charge(
                        &writer,
                        session,
                        port,
                        active_order,
                        charging,
                        charged_seconds,
                        charged_mwh,
                        &stats,
                    )
                    .await;
                    print_stats(&stats);
                    return Ok(());
                }
                _ = tokio::time::sleep(tick) => {
                    if charging {
                        charged_seconds = charged_seconds.saturating_add(if args.speedup >= 1.0 {
                            15
                        } else {
                            5
                        });
                        charged_mwh = charged_mwh.saturating_add(25_000);
                    }
                    send_heartbeat(
                        &writer,
                        &identity,
                        session,
                        charging,
                        port,
                        charged_seconds,
                        charged_mwh,
                        &stats,
                    )
                    .await;
                }
                res = frame::read_frame(&mut reader) => {
                    match res {
                        Ok(inbound) => {
                            stats.frames_received.fetch_add(1, Ordering::Relaxed);
                            handle_command(
                                inbound.command,
                                &inbound.data,
                                &writer,
                                session,
                                &stats,
                                &mut charging,
                                &mut active_order,
                                &mut port,
                                &mut charged_seconds,
                                &mut charged_mwh,
                            )
                            .await;
                        }
                        Err(e) => {
                            stats.conn_errors.fetch_add(1, Ordering::Relaxed);
                            println!("连接断开：{e}；2 秒后重连");
                            alive = false;
                        }
                    }
                }
            }
        }

        // 先关写半让服务端读到 EOF，避免直接 drop 触发 RST。
        {
            let mut w = writer.lock().await;
            let _ = w.shutdown().await;
        }
        if pause_or_stop(&mut stop_rx, Duration::from_secs(2)).await {
            print_stats(&stats);
            return Ok(());
        }
    }
}

/// 处理一条下行命令：B7 启动、B9 停止；其余命令只记录。
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: u8,
    data: &[u8],
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    session: [u8; 6],
    stats: &Stats,
    charging: &mut bool,
    active_order: &mut Option<[u8; 8]>,
    port: &mut u8,
    charged_seconds: &mut u32,
    charged_mwh: &mut u32,
) {
    match command {
        0xB7 => {
            stats.commands_received.fetch_add(1, Ordering::Relaxed);
            match device::parse_start_command(data) {
                Ok((started_port, order, mode, amount, ..)) => {
                    println!(
                        "收到 B7 启动：port={started_port} 模式={mode:?} 额度={amount} 订单={order:02x?}"
                    );
                    *port = started_port;
                    *active_order = Some(order);
                    *charging = true;
                    *charged_seconds = 0;
                    *charged_mwh = 0;
                    let _ = send_command_result(writer, session, 0xB8, started_port, stats).await;
                }
                Err(_) => println!("B7 解析失败，已忽略"),
            }
        }
        0xB9 => {
            stats.commands_received.fetch_add(1, Ordering::Relaxed);
            match device::parse_stop_command(data) {
                Ok(stopped_port) => {
                    println!("收到 B9 停止：port={stopped_port}");
                    let _ = send_command_result(writer, session, 0xBA, stopped_port, stats).await;
                    if let Some(order) = active_order.take() {
                        send_charge_end(
                            writer,
                            session,
                            stopped_port,
                            order,
                            *charged_seconds,
                            *charged_mwh,
                            stats,
                        )
                        .await;
                    }
                    *charging = false;
                }
                Err(_) => println!("B9 解析失败，已忽略"),
            }
        }
        // 平台下发心跳周期：回 A7 接受（载荷 0），节拍沿用本地 idle/charging 规则。
        0xA6 => {
            if let Ok((seconds, port_status)) = device::parse_heartbeat_interval(data) {
                println!("平台设置心跳：{seconds}s，端口遥测={port_status}");
            }
            let reply = frame::Frame::new(0xA7, session, vec![0]);
            if let Ok(raw) = frame::encode(&reply)
                && write_frame(writer, &raw).await.is_ok()
            {
                stats.frames_sent.fetch_add(1, Ordering::Relaxed);
            }
        }
        // A5 是平台对设备心跳的确认，无需处理。
        0xA5 => {}
        other => println!("收到未处理命令 0x{other:02X}"),
    }
}

/// 收尾：充电中则补 BB 结束帧（回显 B7 订单号），否则静默断开。
#[allow(clippy::too_many_arguments)]
async fn finish_charge(
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    session: [u8; 6],
    port: u8,
    active_order: Option<[u8; 8]>,
    charging: bool,
    charged_seconds: u32,
    charged_mwh: u32,
    stats: &Stats,
) {
    if charging && let Some(order) = active_order {
        send_charge_end(
            writer,
            session,
            port,
            order,
            charged_seconds,
            charged_mwh,
            stats,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_derives_valid_protocol_fields() {
        let id = identity_for_board("5348000000000001").unwrap();
        assert_eq!(id.module_id.len(), 16);
        assert_eq!(id.sim.len(), 20);
        assert!(device::build_registration(&id).is_ok());
    }

    #[test]
    fn board_must_be_sixteen_digits() {
        // 8 位业务设备号不是合法 DC589 板号。
        assert!(identity_for_board("12345678").is_err());
        assert!(identity_for_board("53480000000000G1").is_err());
        assert!(identity_for_board("534800000000000").is_err());
    }
}
