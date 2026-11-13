//! DC589 适配器：注册握手、逐帧分派、会话互踢与连接审计。
//! 事件经 [`chargepilot_protocol::Sink`] 交还运行时；持久化成功后才向
//! 设备确认帧。协议字面量不越出本 crate。

use std::net::TcpStream;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chargepilot_protocol::{
    Adapter, BoxFut, CloseReason, Command, CommandKind, DeviceSession, Event, EventType,
    Registration, SessionRecord, SessionRecorder, SessionRouter, Sink,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream as TokioTcpStream;
use tokio::net::tcp::OwnedWriteHalf;

use crate::cards::{parse_card_balance_query, parse_card_swipe};
use crate::commands::{
    build_charge_end_reply, build_fault_reply, build_heartbeat_interval, build_heartbeat_reply,
    build_register_reply, build_time_reply, parse_command_result, parse_heartbeat_set_reply,
    parse_registration,
};
use crate::config::{
    CHARGING_HEARTBEAT_SECONDS, CONFIG_ENCODED_LEN, DEFAULT_HEARTBEAT_SECONDS,
    MISSED_HEARTBEATS_BEFORE_RESET, config_error_field, decode_config, parse_config_ack,
};
use crate::fault::fault_metric;
use crate::frame::{self, Frame};
use crate::measurements::{
    heartbeat_event, parse_charge_end, parse_charging_band, parse_heartbeat,
};

/// 未注册首帧的读取超时。
const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);
/// 写超时。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 对端地址列宽上限。
const REMOTE_ADDR_MAX: usize = 64;

/// DC589 TCP 适配器。
#[derive(Default)]
pub struct TcpAdapter;

impl TcpAdapter {
    /// 空适配器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Adapter for TcpAdapter {
    fn name(&self) -> &'static str {
        "dc589"
    }

    fn serve_conn(
        self: Arc<Self>,
        stream: TcpStream,
        sink: Arc<dyn Sink>,
        recorder: Option<Arc<dyn SessionRecorder>>,
        router: Option<Arc<dyn SessionRouter>>,
    ) -> BoxFut<std::io::Result<()>> {
        Box::pin(async move { serve(stream, sink, recorder, router).await })
    }
}

/// 会话审计：连接流量与未知命令的内存累计，连接结束时落库。
struct SessionAudit {
    bytes_in: AtomicI64,
    bytes_out: AtomicI64,
    frames_in: AtomicI64,
    frames_out: AtomicI64,
    last_seen: Mutex<std::time::SystemTime>,
    unknown: Mutex<Vec<u8>>,
    started_at: std::time::SystemTime,
    remote_addr: String,
}

impl SessionAudit {
    fn new(remote_addr: String) -> Self {
        Self {
            bytes_in: AtomicI64::new(0),
            bytes_out: AtomicI64::new(0),
            frames_in: AtomicI64::new(0),
            frames_out: AtomicI64::new(0),
            last_seen: Mutex::new(std::time::SystemTime::now()),
            unknown: Mutex::new(Vec::new()),
            started_at: std::time::SystemTime::now(),
            remote_addr,
        }
    }

    fn inbound(&self, data_len: usize) {
        self.bytes_in.fetch_add(data_len as i64, Ordering::Relaxed);
        self.frames_in.fetch_add(1, Ordering::Relaxed);
        *self.last_seen.lock().expect("审计锁") = std::time::SystemTime::now();
    }

    fn outbound(&self, data_len: usize) {
        self.bytes_out.fetch_add(data_len as i64, Ordering::Relaxed);
        self.frames_out.fetch_add(1, Ordering::Relaxed);
        *self.last_seen.lock().expect("审计锁") = std::time::SystemTime::now();
    }

    fn mark_unknown(&self, command: u8) {
        let mut list = self.unknown.lock().expect("审计锁");
        if !list.contains(&command) {
            list.push(command);
            list.sort_unstable();
        }
    }
}

/// 单设备会话：串行写半 + 心跳周期控制（命令队列 G3 的写入口）。
/// 可克隆字段全部 Arc 化：DeviceSession 的动态分发要求 send/close 返回
/// 'static future，不借用 &self。
#[derive(Clone)]
pub struct DcSession {
    server_session: Arc<[u8; 6]>,
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    audit: Arc<SessionAudit>,
    /// 当前心跳周期（秒）；读超时为其三倍。
    heartbeat_seconds: Arc<AtomicU32>,
    /// 最近一次下发的心跳周期（去重，避免重复 A6）。
    applied_seconds: Arc<AtomicU32>,
    /// 互踢/停机关闭信号。
    closed: Arc<tokio::sync::Notify>,
    closed_flag: Arc<AtomicU8>,
}

impl DcSession {
    /// 读超时 = 当前申请周期 × 连续缺失阈值。
    fn read_timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.heartbeat_seconds.load(Ordering::Relaxed)))
            * MISSED_HEARTBEATS_BEFORE_RESET
    }

    /// 串行写帧并计入审计。
    async fn write_frame(&self, frame: &Frame) -> std::io::Result<()> {
        write_frame_to(&self.writer, &self.audit, frame).await
    }

    /// 切换心跳周期：仅在实际变化时下发 A6（协议建议按充电状态切换）。
    async fn set_heartbeat(&self, seconds: u32) -> std::io::Result<()> {
        if self.applied_seconds.load(Ordering::Relaxed) == seconds {
            return Ok(());
        }
        let frame = build_heartbeat_interval(
            *self.server_session,
            u16::try_from(seconds).unwrap_or(u16::MAX),
            true,
        )
        .map_err(std::io::Error::other)?;
        self.write_frame(&frame).await?;
        self.applied_seconds.store(seconds, Ordering::Relaxed);
        self.heartbeat_seconds.store(seconds, Ordering::Relaxed);
        Ok(())
    }
}

/// 写帧到底层写半（供 DcSession 与 send 复用）。
async fn write_frame_to(
    writer: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    audit: &SessionAudit,
    frame: &Frame,
) -> std::io::Result<()> {
    let raw = frame::encode(frame).map_err(std::io::Error::other)?;
    let mut guard = writer.lock().await;
    tokio::time::timeout(WRITE_TIMEOUT, guard.write_all(&raw))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "写帧超时"))??;
    drop(guard);
    audit.outbound(frame.data.len());
    Ok(())
}

impl DeviceSession for DcSession {
    fn send(&self, command: Command) -> BoxFut<anyhow::Result<()>> {
        // 克隆共享字段，future 不借用 self。
        let writer = self.writer.clone();
        let audit = self.audit.clone();
        let server_session = *self.server_session;
        let applied = self.applied_seconds.clone();
        let heartbeat = self.heartbeat_seconds.clone();
        Box::pin(async move {
            // 同一 future 内直接调用写帧助手（借用克隆的 Arc 字段）。
            let write = async |frame: &Frame| write_frame_to(&writer, &audit, frame).await;
            // 全零会话号仅允许卡拒绝/余额应答（设备主动查询的无会话回复）。
            if command.session_id == [0u8; 6]
                && !matches!(
                    command.kind,
                    CommandKind::CardDenied | CommandKind::CardBalance
                )
            {
                anyhow::bail!("无效的 5.8.9 载荷：会话号缺失");
            }
            let frame = match command.kind {
                CommandKind::CardDenied => crate::cards::build_card_denied(
                    command.session_id,
                    command.card_number,
                    command.card_invalid,
                    command.card_balance_units,
                )?,
                CommandKind::CardBalance => crate::cards::build_card_balance_reply(
                    command.session_id,
                    command.card_number,
                    !command.card_invalid,
                    command.card_balance_units,
                )?,
                CommandKind::Start => {
                    crate::commands::build_start(&crate::commands::StartCommand {
                        consumer_type: command.consumer_type,
                        card_number: command.card_number,
                        card_balance_units: command.card_balance_units,
                        session: command.session_id,
                        port: command.port,
                        order_bcd: command.order_bcd,
                        mode: crate::commands::ChargeMode::from_code(command.mode)
                            .ok_or_else(|| anyhow::anyhow!("无效的 5.8.9 载荷：充电模式"))?,
                        quantity: command.quantity,
                    })?
                }
                CommandKind::Stop => crate::commands::build_stop(command.session_id, command.port)?,
                CommandKind::Reboot => crate::commands::build_reboot(command.session_id),
            };
            write(&frame).await?;
            // 启动命令后立即切换到充电心跳节奏。
            if command.kind == CommandKind::Start {
                let seconds = u32::from(CHARGING_HEARTBEAT_SECONDS);
                if applied.load(Ordering::Relaxed) != seconds {
                    let frame = build_heartbeat_interval(server_session, seconds as u16, true)
                        .map_err(std::io::Error::other)?;
                    write(&frame).await?;
                    applied.store(seconds, Ordering::Relaxed);
                    heartbeat.store(seconds, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    }

    fn close(&self) -> BoxFut<()> {
        let closed = self.closed.clone();
        let flag = self.closed_flag.clone();
        flag.store(1, Ordering::SeqCst);
        closed.notify_waiters();
        Box::pin(async {})
    }
}

/// 对端地址（截断到列宽）。
fn remote_addr_of(stream: &TcpStream) -> String {
    let addr = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    addr.chars().take(REMOTE_ADDR_MAX).collect()
}

/// 系统时间转 UTC。
fn to_utc(t: std::time::SystemTime) -> chrono::DateTime<chrono::Utc> {
    t.into()
}

/// 连接主流程：注册握手 → 心跳节奏 → 逐帧分派 → 审计收尾。
async fn serve(
    stream: TcpStream,
    sink: Arc<dyn Sink>,
    recorder: Option<Arc<dyn SessionRecorder>>,
    router: Option<Arc<dyn SessionRouter>>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let remote = remote_addr_of(&stream);
    let audit = Arc::new(SessionAudit::new(remote));

    // 一次 split：读半归主循环，写半归会话（命令发送）。
    let tcp = TokioTcpStream::from_std(stream)?;
    let (mut read_half, write_half) = tcp.into_split();

    // 首帧必须是 A0 注册；超时/畸形直接收尾。
    let first =
        match tokio::time::timeout(REGISTER_TIMEOUT, frame::read_frame(&mut read_half)).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                finish(&audit, recorder.as_ref(), "", CloseReason::ReadError).await;
                return Err(e);
            }
            Err(_) => {
                finish(&audit, recorder.as_ref(), "", CloseReason::ReadTimeout).await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "注册超时",
                ));
            }
        };
    let registration = match parse_registration(first.command, &first.data) {
        Ok(r) => r,
        Err(_) => {
            finish(&audit, recorder.as_ref(), "", CloseReason::ProtocolError).await;
            return Err(std::io::Error::other("注册帧无效"));
        }
    };
    let device_id = registration.board_id.clone();
    if let Err(e) = sink
        .register(Registration {
            protocol: "dc589",
            device_id: device_id.clone(),
            hardware_version: registration.hardware_version.clone(),
            software_version: format!(
                "{}:{}",
                registration.software_id, registration.software_version
            ),
            received_at: chrono::Utc::now(),
        })
        .await
    {
        tracing::warn!(device = %device_id, remote = %audit.remote_addr, error = %e, "设备注册被拒绝");
        finish(&audit, recorder.as_ref(), &device_id, CloseReason::Rejected).await;
        return Err(std::io::Error::other("注册被拒绝"));
    }
    audit.inbound(first.data.len());

    // 会话号由服务器生成并随注册回复下发；注册成功即进入心跳节奏。
    let mut server_session = [0u8; 6];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut server_session);
    let session = Arc::new(DcSession {
        server_session: Arc::new(server_session),
        writer: Arc::new(tokio::sync::Mutex::new(write_half)),
        audit: audit.clone(),
        heartbeat_seconds: Arc::new(AtomicU32::new(u32::from(DEFAULT_HEARTBEAT_SECONDS))),
        applied_seconds: Arc::new(AtomicU32::new(0)),
        closed: Arc::new(tokio::sync::Notify::new()),
        closed_flag: Arc::new(AtomicU8::new(0)),
    });
    session
        .write_frame(&build_register_reply(server_session, chrono::Utc::now()))
        .await?;
    // 注册后下发心跳周期并启用端口遥测，避免依赖设备出厂配置；
    // 存在充电端口的设备直接进入 15 秒节奏（旧版 HasChargingPorts 查询）。
    let initial = match sink_has_charging(&sink, &device_id).await {
        Ok(true) => u32::from(CHARGING_HEARTBEAT_SECONDS),
        Ok(false) | Err(_) => u32::from(DEFAULT_HEARTBEAT_SECONDS),
    };
    session
        .write_frame(
            &build_heartbeat_interval(server_session, initial as u16, true)
                .map_err(std::io::Error::other)?,
        )
        .await?;
    session.applied_seconds.store(initial, Ordering::Relaxed);

    // 登记到路由表并互踢旧连接；replaced 标记优先于其他结束原因。
    let replaced = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(router) = router.clone() {
        let flag = replaced.clone();
        let session_for_kick = session.clone();
        let _ = router
            .attach(
                &device_id,
                session_for_kick.clone() as Arc<dyn DeviceSession>,
                Arc::new(move || {
                    flag.store(true, Ordering::SeqCst);
                    session_for_kick.closed_flag.store(1, Ordering::SeqCst);
                    session_for_kick.closed.notify_waiters();
                }),
            )
            .await;
    }
    tracing::info!(device = %device_id, session = %hex_session(&server_session),
        remote = %audit.remote_addr, heartbeat_seconds = initial, "设备上线");

    let outcome = run_loop(&mut read_half, &session, &sink, &device_id).await;

    // 结束原因判定：互踢 > 停机 > 读超时 > 协议错误 > 设备关闭。
    let reason = if replaced.load(Ordering::SeqCst) {
        CloseReason::Replaced
    } else if session.closed_flag.load(Ordering::SeqCst) == 1 {
        CloseReason::ContextEnded
    } else {
        match &outcome {
            Ok(()) => CloseReason::DeviceClosed,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => CloseReason::ReadTimeout,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => CloseReason::ReadTimeout,
            // 对端正常关闭（EOF）按设备断开处理，不记为协议错误。
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => CloseReason::DeviceClosed,
            Err(_) => CloseReason::ReadError,
        }
    };
    if let Some(router) = router {
        router
            .detach(&device_id, &(session.clone() as Arc<dyn DeviceSession>))
            .await;
    }
    tracing::info!(device = %device_id, session = %hex_session(&server_session), reason = reason.as_str(), "设备离线");
    finish(&audit, recorder.as_ref(), &device_id, reason).await;
    outcome
}

/// 按设备是否在充电选择初始心跳节奏（sink 可选能力，未实现按空闲处理）。
async fn sink_has_charging(sink: &Arc<dyn Sink>, device_id: &str) -> anyhow::Result<bool> {
    // 单体语境：充电状态判定在运行时的会话缓存中；适配器只感知 sink。
    // 无充电缓存能力时默认空闲 60 秒（注册后首个 A4 会立即校正节奏）。
    let _ = (sink, device_id);
    Ok(false)
}

/// 逐帧分派主循环：返回连接结束的 IO 结果。
async fn run_loop(
    read_half: &mut tokio::net::tcp::OwnedReadHalf,
    session: &Arc<DcSession>,
    sink: &Arc<dyn Sink>,
    device_id: &str,
) -> std::io::Result<()> {
    loop {
        let read = tokio::time::timeout(session.read_timeout(), frame::read_frame(read_half));
        tokio::select! {
            biased;
            _ = session.closed.notified() => {
                return Err(std::io::Error::other("连接被替换或停机关闭"));
            }
            result = read => match result {
                Ok(Ok(f)) => {
                    audit_inbound(session, &f);
                    if let Err(e) = dispatch(session, sink, device_id, f).await {
                        return Err(e);
                    }
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "读超时",
                    ));
                }
            },
        }
    }
}

/// 计入入站审计。
fn audit_inbound(session: &DcSession, f: &Frame) {
    session.audit.inbound(f.data.len());
}

/// 逐帧分派：事件落库成功后才应答设备（旧版语义）。
async fn dispatch(
    session: &Arc<DcSession>,
    sink: &Arc<dyn Sink>,
    device_id: &str,
    f: Frame,
) -> std::io::Result<()> {
    let received = chrono::Utc::now();
    let make_event = |kind: EventType| Event {
        device_id: device_id.to_string(),
        kind: Some(kind),
        received_at: Some(received),
        raw_payload: f.data.clone(),
        session_id: f.session,
        ..Event::default()
    };
    match f.command {
        // A4 心跳：落库 → A5 → 按充电状态切换节奏。
        0xA4 => {
            let hb = parse_heartbeat(&f.data).map_err(std::io::Error::other)?;
            if hb.board_id != device_id {
                return Err(std::io::Error::other("心跳板号不匹配"));
            }
            let mut event = heartbeat_event(device_id, &hb, received, f.data.clone(), f.session);
            event.signal = hb.signal;
            sink.record(event).await.map_err(io_of)?;
            session
                .write_frame(&build_heartbeat_reply(f.session))
                .await?;
            if hb.has_port_status {
                let charging = hb
                    .charging_ports
                    .iter()
                    .any(|p| p.charged_seconds > 0 || p.power_deci_watts > 0)
                    || hb.port_states.iter().any(|&s| s == 1);
                let seconds = if charging {
                    u32::from(CHARGING_HEARTBEAT_SECONDS)
                } else {
                    u32::from(DEFAULT_HEARTBEAT_SECONDS)
                };
                session.set_heartbeat(seconds).await?;
            }
        }
        // B6 在线卡刷卡：落库；授权/拒绝由运行时经会话下发（B7/BD）。
        0xB6 => {
            let swipe = parse_card_swipe(f.command, &f.data).map_err(std::io::Error::other)?;
            let mut event = make_event(EventType::CardSwipe);
            event.port = swipe.port;
            event.card_number = swipe.card_number;
            event.event_id = new_event_id();
            sink.record(event).await.map_err(io_of)?;
        }
        // D0 卡余额查询：落库；应答由运行时经会话下发（D1）。
        0xD0 => {
            let card =
                parse_card_balance_query(f.command, &f.data).map_err(std::io::Error::other)?;
            let mut event = make_event(EventType::CardBalanceQuery);
            event.card_number = card;
            event.event_id = new_event_id();
            sink.record(event).await.map_err(io_of)?;
        }
        // A8 校时请求：落库后直接 A9（无需业务参与）。
        0xA8 => {
            if f.data.len() != 6 {
                return Err(std::io::Error::other("校时请求无效"));
            }
            let event = make_event(EventType::TimeSync);
            sink.record(event).await.map_err(io_of)?;
            session
                .write_frame(&build_time_reply(f.session, chrono::Utc::now()))
                .await?;
        }
        // B8/BA 启停结果：落库（命令等待方消费）。
        0xB8 | 0xBA => {
            let result = parse_command_result(f.command, &f.data).map_err(std::io::Error::other)?;
            let mut event = make_event(if f.command == 0xB8 {
                EventType::StartResult
            } else {
                EventType::StopResult
            });
            event.port = result.port;
            event.result_code = result.code;
            sink.record(event).await.map_err(io_of)?;
        }
        // BB 结束订单：落库 → BC 确认。
        0xBB => {
            let end = parse_charge_end(&f.data).map_err(std::io::Error::other)?;
            let event = crate::measurements::charge_end_event(
                device_id,
                &end,
                received,
                f.data.clone(),
                f.session,
            );
            sink.record(event).await.map_err(io_of)?;
            session
                .write_frame(&build_charge_end_reply(f.session, end.port))
                .await?;
        }
        // C0 故障上报：落库 → C1 确认。
        0xC0 => {
            if f.data.len() != 5 {
                return Err(std::io::Error::other("故障帧无效"));
            }
            let port = f.data[0];
            let mut event = make_event(EventType::Fault);
            event.port = port;
            event.fault_code = f.data[1];
            let metric = fault_metric(port, event.fault_code);
            tracing::warn!(device = %device_id, port, metric = %metric.metric,
                severity = metric.severity.as_str(), "设备故障上报");
            sink.record(event).await.map_err(io_of)?;
            session
                .write_frame(&build_fault_reply(f.session, port))
                .await?;
        }
        // B4 本地启动上报：落库（平台补全订单）。
        0xB4 => {
            if f.data.len() != 36 {
                return Err(std::io::Error::other("本地启动帧无效"));
            }
            let port = f.data[0];
            let mut event = make_event(EventType::LocalStart);
            event.port = port;
            event.event_id = new_event_id();
            // 订单号（8 字节 BCD）与消费类型按 B4 载荷偏移解出。
            if let Ok(order) = crate::decode_bcd(&f.data[2..10]) {
                event.order_number = order;
            }
            event.consumer_type = f.data[29];
            event.started_at = crate::decode_civil_time(&f.data[10..16]).ok();
            sink.record(event).await.map_err(io_of)?;
            // B5 本地启动确认（0=收到）。
            session
                .write_frame(&Frame::new(0xB5, f.session, vec![0, port]))
                .await?;
        }
        // A3 远程控制结果（复位/升级成功时厂商不回复）。
        0xA3 => {
            if f.data.len() != 2 {
                return Err(std::io::Error::other("远程控制结果无效"));
            }
            let mut event = make_event(EventType::RemoteResult);
            event.result_code = f.data[1];
            sink.record(event).await.map_err(io_of)?;
        }
        // C2 分档上报：无需应答。
        0xC2 => {
            let meter = parse_charging_band(&f.data).map_err(std::io::Error::other)?;
            let mut event = make_event(EventType::Telemetry);
            event.port = meter.port;
            event.power_deci_watts = meter.power_deci_watts;
            event.charging_ports = vec![meter];
            sink.record(event).await.map_err(io_of)?;
        }
        // A7/C4/C6/E1 配置类应答：记录结果码（拒绝原因供运维定位）。
        0xA7 | 0xC4 | 0xC6 | 0xE1 => {
            let mut event = make_event(EventType::ConfigResult);
            match f.command {
                0xA7 => match parse_heartbeat_set_reply(f.command, &f.data) {
                    Ok(true) => event.result_code = 0,
                    Ok(false) => event.result_code = 1,
                    Err(_) => event.result_code = 0xFF,
                },
                0xC4 => match parse_config_ack(f.command, &f.data) {
                    Ok(code) if code == 0 => event.result_code = 0,
                    Ok(code) => {
                        event.result_code = code;
                        if let Some(field) = config_error_field(code) {
                            tracing::warn!(device = %device_id, code, field, "设备拒绝参数下发");
                        }
                    }
                    Err(_) => event.result_code = 0xFF,
                },
                0xC6 => {
                    if f.data.len() != CONFIG_ENCODED_LEN
                        || decode_config(f.command, &f.data).is_err()
                    {
                        event.result_code = 0xFF;
                    }
                }
                _ => event.result_code = 0xFF,
            }
            sink.record(event).await.map_err(io_of)?;
        }
        // 未实现的命令记录后跳过，保留连接以兼容固件的可选命令。
        other => {
            session.audit.mark_unknown(other);
            tracing::debug!(device = %device_id, command = format!("{other:#04X}"), "未实现的设备命令");
        }
    }
    Ok(())
}

/// anyhow 错误转 IO 错误。
fn io_of(e: anyhow::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// 会话号的十六进制文本。
fn hex_session(session: &[u8; 6]) -> String {
    session.iter().map(|b| format!("{b:02x}")).collect()
}

/// 新事件标识（UUID v4 形态；依赖 workspace rand）。
fn new_event_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 连接收尾：保存会话审计记录（落库失败仅记录，不覆盖通信错误）。
async fn finish(
    audit: &SessionAudit,
    recorder: Option<&Arc<dyn SessionRecorder>>,
    device_id: &str,
    reason: CloseReason,
) {
    let Some(recorder) = recorder else {
        return;
    };
    let ended = chrono::Utc::now();
    let record = SessionRecord {
        // 会话号在审计创建时尚未生成；以设备号+起始时间保证可区分。
        session_id: format!(
            "{}-{}",
            device_id,
            audit
                .started_at
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ),
        device_id: device_id.to_string(),
        remote_addr: audit.remote_addr.clone(),
        started_at: Some(to_utc(audit.started_at)),
        last_active: Some(to_utc(*audit.last_seen.lock().expect("审计锁"))),
        ended_at: Some(ended),
        close_reason: reason.as_str().to_string(),
        bytes_in: audit.bytes_in.load(Ordering::Relaxed),
        bytes_out: audit.bytes_out.load(Ordering::Relaxed),
        frames_in: audit.frames_in.load(Ordering::Relaxed),
        frames_out: audit.frames_out.load(Ordering::Relaxed),
        unknown_commands: audit.unknown.lock().expect("审计锁").clone(),
    };
    if let Err(e) = recorder.record_session(record).await {
        tracing::warn!(device = %device_id, error = %e, "会话审计落库失败");
    }
}
