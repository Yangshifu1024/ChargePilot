//! headless 批量模拟器（`simulator-batch` 子命令）：一条命令拉起 N 台虚拟
//! 设备跑完整协议交互，作为容量验收的加载源（阶段 07）。
//!
//! 与网关共用 `protocol-dc589` 帧层（同一实现即契约，互逆免费获得协议覆盖）。
//! 充电负载表现为心跳携带的充电端口遥测（网关侧缓冲/落库/解析负载真实发生）；
//! 完整订单生命周期由闭环测试（gateway-runtime/tests/closed_loop.rs）承担。
//! 本工具是测试工具链，不属于生产服务路径，但随同一镜像交付便于现场排障。

use chargepilot_protocol::PortTelemetry;
use chargepilot_protocol_dc589::device::{self, ChargeEndReport, DeviceIdentity, PortStatus};
use chargepilot_protocol_dc589::frame::{self};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt as _, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::watch;

/// 批量模拟器参数（clap 平铺到子命令）。
#[derive(Debug, Clone, clap::Args)]
pub struct BatchArgs {
    /// 虚拟设备总数（每台一个 tokio 任务，一台一条 TCP 连接）。
    #[arg(long, default_value_t = 2000)]
    pub devices: u32,
    /// 同时充电台数（心跳携带充电遥测；不超过设备总数）。
    #[arg(long, default_value_t = 1000)]
    pub charging: u32,
    /// 心跳/定时器加速倍率（按倍率缩放间隔，不改协议字节内容）。
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,
    /// 网关设备接入地址（host:port，如 127.0.0.1:9100）。
    #[arg(long)]
    pub target_addr: String,
    /// 运行时长（秒）；到时全体设备按序收尾并输出统计。
    #[arg(long, default_value_t = 300)]
    pub duration: u64,
    /// 异常注入比例（0.0~1.0 的设备执行迟到应答/半开挂住/重连风暴）。
    #[arg(long, default_value_t = 0.0)]
    pub chaos_ratio: f64,
    /// 随机种子：同种子同设备号、同 chaos 分派，帧序列可复现。
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// 预导入设备事实（批量 INSERT device_meta），否则未导入设备会被网关拒绝。
    #[arg(long, requires = "database_url")]
    pub provision: bool,
    /// 数据库连接（--provision 与注册核对用；缺省读 DATABASE_URL）。
    #[arg(long)]
    pub database_url: Option<String>,
}

/// 确定性板号：前缀 5348 + 序号 12 位补零，共 16 位十进制（8 字节 BCD）。
/// 同种子参数下设备号恒定，可重复压测定位问题设备。
pub fn board_number(index: u32) -> String {
    format!("5348{index:012}")
}

/// 确定性模块号/SIM（长度满足协议编码即可，值随序号变化便于区分设备）。
fn identity_for(index: u32) -> DeviceIdentity {
    DeviceIdentity {
        board_id: board_number(index),
        hardware_version: "SH10HP01".into(),
        software_id: "SH10SP02".into(),
        software_version: 107,
        module_id: format!("086312107741{index:04}"),
        sim: format!("898604041018{index:08}"),
        signal: 20,
    }
}

/// 按加速倍率缩放心跳间隔；下限 100ms 防止 0/负值与忙转。
pub fn scaled_interval(base_secs: u64, speedup: f64) -> Duration {
    let factor = speedup.max(1.0);
    let secs = (base_secs as f64 / factor).max(0.1);
    Duration::from_secs_f64(secs)
}

/// 异常注入风格：按 chaos_ratio 概率抽取，比例内再按固定权重分三类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosStyle {
    /// 正常设备。
    Normal,
    /// 命令应答延迟 1~5 秒（迟到回执路径）。
    SlowAck,
    /// 充电中途停止发心跳但保持连接（半开连接，等待服务端读超时踢）。
    HalfOpen,
    /// 首次注册成功后立即静默断连并以同板号重连（注册风暴重放）。
    Replay,
}

/// 从已播种 RNG 抽取 chaos 风格；ratio=0 恒 Normal（确定性可单测）。
pub fn pick_chaos(rng: &mut StdRng, ratio: f64) -> ChaosStyle {
    if !(0.0..=1.0).contains(&ratio) || rng.random::<f64>() >= ratio {
        return ChaosStyle::Normal;
    }
    match rng.random::<f64>() {
        r if r < 0.3 => ChaosStyle::SlowAck,
        r if r < 0.7 => ChaosStyle::HalfOpen,
        _ => ChaosStyle::Replay,
    }
}

/// 运行统计（原子计数；跨任务无锁累加，收尾快照输出）。
#[derive(Debug, Default)]
pub struct Stats {
    pub frames_sent: AtomicU64,
    pub frames_received: AtomicU64,
    pub registrations_ok: AtomicU64,
    pub registrations_rejected: AtomicU64,
    pub commands_received: AtomicU64,
    pub acks_sent: AtomicU64,
    pub end_frames_sent: AtomicU64,
    pub reconnects: AtomicU64,
    pub conn_errors: AtomicU64,
    pub chaos_events: AtomicU64,
}

impl Stats {
    fn snapshot(&self) -> String {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let regs = g(&self.registrations_ok);
        let rejected = g(&self.registrations_rejected);
        let ack_rate = if regs + rejected > 0 {
            format!("{:.1}%", regs as f64 * 100.0 / (regs + rejected) as f64)
        } else {
            "n/a".into()
        };
        format!(
            "发送帧 {} / 接收帧 {}\n注册成功 {} / 拒绝 {}（成功率 {ack_rate}）\n\
             收到命令 {} / 应答 {} / 结束帧 {}\n重连 {} / 连接错误 {} / 异常注入事件 {}",
            g(&self.frames_sent),
            g(&self.frames_received),
            regs,
            rejected,
            g(&self.commands_received),
            g(&self.acks_sent),
            g(&self.end_frames_sent),
            g(&self.reconnects),
            g(&self.conn_errors),
            g(&self.chaos_events),
        )
    }
}

/// 单台虚拟设备的完整会话循环：连接 → 注册 → 心跳/命令应答，断连重连。
/// 返回本次任务累计注册成功次数（供总体成功率统计）。
async fn device_task(
    index: u32,
    args: Arc<BatchArgs>,
    stats: Arc<Stats>,
    shutdown: watch::Receiver<bool>,
) {
    let mut rng = StdRng::seed_from_u64(args.seed ^ (index as u64).wrapping_mul(0x9E37_79B9));
    let chaos = pick_chaos(&mut rng, args.chaos_ratio);
    let identity = identity_for(index);
    let is_charging = index < args.charging;
    let idle_tick = scaled_interval(60, args.speedup);
    let charging_tick = scaled_interval(15, args.speedup);
    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let mut replay_done = chaos != ChaosStyle::Replay;
    // 上电抖动：设备启动时序分散（网关 accept 限速 200/s，2000 台冷启动
    // 挤同一窗口会拖长注册队列导致应答超时；真实设备上电天然错峰）。
    tokio::time::sleep(Duration::from_millis(
        rng.random_range(0..u64::from(args.devices.min(4000)) * 5),
    ))
    .await;

    loop {
        if *shutdown.borrow() || Instant::now() >= deadline {
            return;
        }
        let Ok(stream) = TcpStream::connect(&args.target_addr).await else {
            stats.conn_errors.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        let (mut reader, writer) = stream.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(writer)));

        // 注册：全零会话号 A0；A1 帧头带回服务端分配的会话号。
        let Ok(reg) = device::build_registration(&identity) else {
            return;
        };
        if write_frame(&writer, &frame::encode(&reg).unwrap())
            .await
            .is_err()
        {
            stats.conn_errors.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(rng.random_range(500..2000))).await;
            continue;
        }
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
        let Some(session) = expect_register_ok(&mut reader, &stats, Duration::from_secs(10)).await
        else {
            // 注册应答异常（拒绝/协议错误/超时）：计数并退避重连，禁止无退避风暴。
            stats.conn_errors.fetch_add(1, Ordering::Relaxed);
            stats.reconnects.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(rng.random_range(500..2000))).await;
            continue;
        };
        stats.registrations_ok.fetch_add(1, Ordering::Relaxed);

        // 注册风暴重放：立即静默断连再重连（互踢路径负载）。
        if !replay_done {
            replay_done = true;
            stats.chaos_events.fetch_add(1, Ordering::Relaxed);
            stats.reconnects.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // 会话主循环：心跳定时 + 命令应答 + 半开挂住。
        let started = Instant::now();
        let mut charged_seconds: u32 = 0;
        let mut charged_mwh: u32 = 0;
        let mut half_open = false;
        loop {
            if *shutdown.borrow() || Instant::now() >= deadline {
                // 收尾：充电台补结束帧后断连（真实设备收尾顺序）。
                // 先关写半让服务端读到 EOF，短暂等待排空在途应答；
                // 直接 drop 会因接收缓冲残留触发 RST（服务端记 read_error）。
                if is_charging && !half_open {
                    send_charge_end(&writer, session, charged_seconds, charged_mwh, &stats).await;
                }
                let mut w = writer.lock().await;
                let _ = w.shutdown().await;
                drop(w);
                tokio::time::sleep(Duration::from_millis(150)).await;
                return;
            }
            let tick = if is_charging {
                charging_tick
            } else {
                idle_tick
            };
            let read = tokio::time::timeout(tick, frame::read_frame(&mut reader)).await;
            match read {
                Err(_elapsed) => {
                    // 读超时 = 心跳节拍：半开设备停发（服务端读超时踢它）。
                    if half_open {
                        continue;
                    }
                    if is_charging {
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
                        is_charging,
                        charged_seconds,
                        charged_mwh,
                        &stats,
                    )
                    .await;
                }
                Ok(Err(_)) => {
                    stats.conn_errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                Ok(Ok(frame)) => {
                    stats.frames_received.fetch_add(1, Ordering::Relaxed);
                    if let Some(cmd) =
                        handle_inbound(frame.command, &writer, session, chaos, &stats).await
                    {
                        // B9 停止：应答后补结束帧并转空闲（半开路径除外）。
                        if cmd == 0xB9 && !half_open {
                            send_charge_end(&writer, session, charged_seconds, charged_mwh, &stats)
                                .await;
                            {
                                let mut w = writer.lock().await;
                                let _ = w.shutdown().await;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            return;
                        }
                    }
                    // 半开设备运行过半后停止发心跳（挂住等读超时踢）。
                    if chaos == ChaosStyle::HalfOpen
                        && !half_open
                        && started.elapsed() > Duration::from_secs(args.duration / 2)
                    {
                        half_open = true;
                        stats.chaos_events.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        // 断连退避重连（挂住的半开与重放已在上层处理）。
        stats.reconnects.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(rng.random_range(500..2000))).await;
    }
}

/// 写一帧（带长度头与校验由 encode 完成），失败返回连接错误。
async fn write_frame(
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    raw: &[u8],
) -> std::io::Result<()> {
    let mut w = writer.lock().await;
    w.write_all(raw).await?;
    w.flush().await
}

/// 等注册应答：A1 成功回帧头会话号；拒绝、协议错误或超时返回 None。
/// read_frame 流式解析，容忍 A1/A6 粘包与分段到达。
async fn expect_register_ok(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    stats: &Stats,
    timeout: Duration,
) -> Option<[u8; 6]> {
    let frame = tokio::time::timeout(timeout, frame::read_frame(reader))
        .await
        .ok()?
        .ok()?;
    stats.frames_received.fetch_add(1, Ordering::Relaxed);
    match frame.command {
        0xA1 => {
            let (status, _) = device::parse_register_reply(0xA1, &frame.data).ok()?;
            if status != 0 {
                stats.registrations_rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(frame.session)
        }
        _ => {
            stats.registrations_rejected.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// 发送心跳：充电台携带单端口遥测（时长/电量/功率递增），空闲台短格式。
async fn send_heartbeat(
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    identity: &DeviceIdentity,
    session: [u8; 6],
    charging: bool,
    charged_seconds: u32,
    charged_mwh: u32,
    stats: &Stats,
) {
    let frame = if charging {
        let status = PortStatus {
            device_status: 0,
            voltage_v: 220,
            temperature_c: 25,
            port_states: vec![0x01],
            charging: vec![PortTelemetry {
                port: 1,
                remaining_secs: 36_000_u32.saturating_sub(charged_seconds),
                charged_seconds,
                remaining_mwh: 0,
                charged_mwh,
                power_deci_watts: 1500,
            }],
        };
        device::build_heartbeat(identity, Some(&status))
            .ok()
            .map(|mut f| {
                f.session = session;
                f
            })
    } else {
        device::build_heartbeat(identity, None).ok().map(|mut f| {
            f.session = session;
            f
        })
    };
    let Some(frame) = frame else { return };
    if write_frame(writer, &frame::encode(&frame).unwrap())
        .await
        .is_ok()
    {
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// 发送 BB 结束帧（设备侧时间取当前时刻，电量/时长来自遥测累计）。
#[allow(clippy::too_many_arguments)]
async fn send_charge_end(
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    session: [u8; 6],
    charged_seconds: u32,
    charged_mwh: u32,
    stats: &Stats,
) {
    let now = chrono::Utc::now();
    let report = ChargeEndReport {
        port: 1,
        order_bcd: [0; 8], // 无订单场景：全零订单号仅作设备事件审计
        started_at: now - chrono::Duration::seconds(i64::from(charged_seconds)),
        ended_at: now,
        charged_mwh,
        power_deci_watts: 0,
        stop_reason: 7,
        consumer_type: 2,
    };
    let Ok(mut f) = device::build_charge_end(&report) else {
        return;
    };
    f.session = session;
    if write_frame(writer, &frame::encode(&f).unwrap())
        .await
        .is_ok()
    {
        stats.end_frames_sent.fetch_add(1, Ordering::Relaxed);
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// 处理平台下发命令：B7 → B8 成功应答；B9 → BA 受理应答。
/// 返回收到的命令类别（0xB9 表示已停止）。
async fn handle_inbound(
    command: u8,
    writer: &Arc<tokio::sync::Mutex<BufWriter<OwnedWriteHalf>>>,
    session: [u8; 6],
    chaos: ChaosStyle,
    stats: &Stats,
) -> Option<u8> {
    let (reply_command, is_command) = match command {
        0xB7 => (0xB8_u8, true),
        0xB9 => (0xBA_u8, true),
        _ => return None,
    };
    stats.commands_received.fetch_add(1, Ordering::Relaxed);
    if chaos == ChaosStyle::SlowAck {
        stats.chaos_events.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(
            StdRng::seed_from_u64(u64::from(reply_command)).random_range(1000..5000),
        ))
        .await;
    }
    let mut reply = device::build_command_result(reply_command, 0, 1).ok()?;
    reply.session = session;
    if write_frame(writer, &frame::encode(&reply).unwrap())
        .await
        .is_ok()
    {
        stats.acks_sent.fetch_add(1, Ordering::Relaxed);
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
    is_command.then_some(command)
}

/// 批量预导入设备事实（device_meta 单行即可注册成功）。
async fn provision_devices(args: &BatchArgs) -> anyhow::Result<()> {
    let url = args
        .database_url
        .clone()
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .ok_or_else(|| anyhow::anyhow!("--provision 需要连接串：--database-url 或 DATABASE_URL"))?;
    let pool = chargepilot_storage::open_pool(&url, 2).await?;
    let mut tx = pool.begin().await?;
    for index in 0..args.devices {
        sqlx::query(
            "INSERT IGNORE INTO `device_meta` (`device_id`, `protocol_adapter`) \
             VALUES (?, 'dc589')",
        )
        .bind(board_number(index))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    println!("预导入完成：{} 台设备事实", args.devices);
    pool.close().await;
    Ok(())
}

/// 子命令入口：预导入（可选）→ 拉起设备任务 → 到时收尾统计。
/// 退出码语义：注册成功率低于 80% 或连接错误占比过高返回错误。
pub async fn run(args: BatchArgs) -> anyhow::Result<()> {
    if args.charging > args.devices {
        anyhow::bail!("--charging 不得超过 --devices");
    }
    if args.provision {
        provision_devices(&args).await?;
    }
    let stats = Arc::new(Stats::default());
    let args = Arc::new(args);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let total = u64::from(args.devices);
    let mut tasks = Vec::with_capacity(total as usize);
    for index in 0..args.devices {
        tasks.push(tokio::spawn(device_task(
            index,
            args.clone(),
            stats.clone(),
            shutdown_rx.clone(),
        )));
    }
    // Ctrl-C 或到时统一收尾。
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("收到中断信号，开始收尾…");
        }
        _ = tokio::time::sleep(Duration::from_secs(args.duration)) => {}
    }
    let _ = shutdown_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    println!("=== 模拟器统计 ===");
    println!("{}", stats.snapshot());
    // 退出码反映成败：注册成功率低于 80% 视为失败（自动化验收可判定）。
    let regs = stats.registrations_ok.load(Ordering::Relaxed);
    let rejected = stats.registrations_rejected.load(Ordering::Relaxed);
    if regs + rejected > 0 && regs * 100 < (regs + rejected) * 80 {
        anyhow::bail!("注册成功率低于 80%：成功 {regs} / 拒绝 {rejected}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_numbers_are_deterministic_and_16_digits() {
        assert_eq!(board_number(0), "5348000000000000");
        assert_eq!(board_number(1), "5348000000000001");
        assert_eq!(board_number(1), board_number(1));
        assert_eq!(board_number(999_999_999).len(), 16);
        // 板号可被协议层接受（16 位十进制 BCD）。
        assert!(device::build_registration(&identity_for(7)).is_ok());
    }

    #[test]
    fn scaled_interval_respects_speedup_with_floor() {
        assert_eq!(scaled_interval(60, 1.0), Duration::from_secs(60));
        assert_eq!(scaled_interval(60, 15.0), Duration::from_secs(4));
        assert_eq!(scaled_interval(15, 15.0), Duration::from_secs(1));
        // 加速下限 100ms，speedup 无效值按 1 处理。
        assert_eq!(scaled_interval(60, 0.0), Duration::from_secs(60));
        assert_eq!(scaled_interval(1, 100_000.0), Duration::from_millis(100));
    }

    #[test]
    fn chaos_pick_is_deterministic_and_bounded() {
        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(pick_chaos(&mut rng, 0.0), ChaosStyle::Normal);
        let mut rng = StdRng::seed_from_u64(7);
        assert_eq!(pick_chaos(&mut rng, 1.5), ChaosStyle::Normal);
        // ratio=1：全部注入，且分派可复现。
        let styles: Vec<ChaosStyle> = (0..8)
            .map(|seed| pick_chaos(&mut StdRng::seed_from_u64(seed), 1.0))
            .collect();
        assert_eq!(
            styles,
            (0..8)
                .map(|seed| pick_chaos(&mut StdRng::seed_from_u64(seed), 1.0))
                .collect::<Vec<_>>()
        );
        assert!(styles.iter().all(|s| *s != ChaosStyle::Normal));
    }

    #[test]
    fn stats_snapshot_reports_key_lines() {
        let stats = Stats::default();
        stats.registrations_ok.store(90, Ordering::Relaxed);
        stats.registrations_rejected.store(10, Ordering::Relaxed);
        stats.frames_sent.store(1000, Ordering::Relaxed);
        let text = stats.snapshot();
        assert!(text.contains("注册成功 90 / 拒绝 10"));
        assert!(text.contains("90.0%"));
        assert!(text.contains("发送帧 1000"));
    }
}
