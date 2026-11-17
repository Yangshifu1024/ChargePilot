//! 适配器回环集成测试：真实 TCP 连接驱动注册握手、心跳应答与节奏切换、
//! 刷卡事件、同设备互踢、会话审计收尾。
//! 测试在持锁状态下 await；进程内互斥锁尚未统一为跨进程端口锁，故豁免
//! `clippy::await_holding_lock`。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

use std::sync::{Arc, Mutex};

use chargepilot_protocol::{Adapter as _, Event, Registration, SessionRecord, SessionRouter as _};
use chargepilot_protocol_dc589::adapter::TcpAdapter;
use chargepilot_protocol_dc589::frame::{self, Frame};
use chargepilot_protocol_dc589::{china_offset, device};
use chrono::TimeZone as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// 内存事件收集 sink。
#[derive(Default)]
struct MemSink {
    registrations: Mutex<Vec<Registration>>,
    events: Mutex<Vec<Event>>,
}

impl chargepilot_protocol::Sink for MemSink {
    fn register(
        &self,
        registration: Registration,
    ) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        self.registrations.lock().unwrap().push(registration);
        Box::pin(async { Ok(()) })
    }
    fn record(&self, event: Event) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        self.events.lock().unwrap().push(event);
        Box::pin(async { Ok(()) })
    }
}

/// 内存会话审计存储。
#[derive(Default)]
struct MemRecorder {
    records: Mutex<Vec<SessionRecord>>,
}

impl chargepilot_protocol::SessionRecorder for MemRecorder {
    fn record_session(
        &self,
        record: SessionRecord,
    ) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        self.records.lock().unwrap().push(record);
        Box::pin(async { Ok(()) })
    }
}

fn identity() -> device::DeviceIdentity {
    device::DeviceIdentity {
        board_id: "5348240514082652".into(),
        hardware_version: "SH10HP01".into(),
        software_id: "SH10SP02".into(),
        software_version: 107,
        module_id: "0863121077410587".into(),
        sim: "898604041018C0013999".into(),
        signal: 17,
    }
}

/// 起一个接入协程，返回监听地址。
async fn serve_one(
    sink: Arc<MemSink>,
    recorder: Arc<MemRecorder>,
    router: Option<Arc<chargepilot_gateway_runtime::SessionRegistry>>,
) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let adapter = Arc::new(TcpAdapter::new());
    tokio::spawn(async move {
        let router: Option<Arc<dyn chargepilot_protocol::SessionRouter>> = router.map(|r| r as _);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let adapter = adapter.clone();
            let sink = sink.clone();
            let recorder = recorder.clone();
            let router = router.clone();
            tokio::spawn(async move {
                let _ = adapter
                    .serve_conn(stream.into_std().unwrap(), sink, Some(recorder), router)
                    .await;
            });
        }
    });
    addr
}

/// 读一条帧（带超时）。
async fn read_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Frame {
    tokio::time::timeout(std::time::Duration::from_secs(5), frame::read_frame(reader))
        .await
        .expect("读帧超时")
        .expect("读帧失败")
}

#[tokio::test]
async fn register_heartbeat_and_audit() {
    let sink = Arc::new(MemSink::default());
    let recorder = Arc::new(MemRecorder::default());
    let addr = serve_one(sink.clone(), recorder.clone(), None).await;

    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    // A0 注册。
    let reg = device::build_registration(&identity()).unwrap();
    conn.write_all(&frame::encode(&reg).unwrap()).await.unwrap();

    // A1 注册回复 + A6 心跳设置。
    let a1 = read_frame(&mut conn).await;
    assert_eq!(a1.command, 0xA1);
    let (status, at) = device::parse_register_reply(0xA1, &a1.data).unwrap();
    assert_eq!(status, 0);
    assert!(at > chrono::Utc::now() - chrono::Duration::minutes(1));
    let a6 = read_frame(&mut conn).await;
    assert_eq!(a6.command, 0xA6);
    assert_eq!(
        device::parse_heartbeat_interval(&a6.data).unwrap(),
        (60, true),
        "默认空闲 60 秒 + 端口遥测开"
    );

    // A4 心跳（带端口状态、无充电）→ A5，节奏保持 60 秒（不重复下发 A6）。
    let hb = device::build_heartbeat(
        &identity(),
        Some(&device::PortStatus {
            device_status: 0,
            voltage_v: 220,
            temperature_c: 25,
            port_states: vec![0, 0],
            charging: vec![],
        }),
    )
    .unwrap();
    conn.write_all(&frame::encode(&hb).unwrap()).await.unwrap();
    let a5 = read_frame(&mut conn).await;
    assert_eq!(a5.command, 0xA5);

    // 充电中的心跳：2 号端口充电 → A6 切到 15 秒。
    let hb = device::build_heartbeat(
        &identity(),
        Some(&device::PortStatus {
            device_status: 0,
            voltage_v: 220,
            temperature_c: 25,
            port_states: vec![0, 1],
            charging: vec![chargepilot_protocol::PortTelemetry {
                port: 2,
                remaining_secs: 600,
                charged_seconds: 65,
                power_deci_watts: 1000,
                ..Default::default()
            }],
        }),
    )
    .unwrap();
    conn.write_all(&frame::encode(&hb).unwrap()).await.unwrap();
    let a5 = read_frame(&mut conn).await;
    assert_eq!(a5.command, 0xA5);
    let a6 = read_frame(&mut conn).await;
    assert_eq!(a6.command, 0xA6);
    assert_eq!(
        device::parse_heartbeat_interval(&a6.data).unwrap(),
        (15, true),
        "充电状态切 15 秒"
    );

    // B6 刷卡 → 事件落库。
    let swipe = Frame::new(0xB6, [0; 6], vec![2, 1, 2, 0x0C, 1, 10, 0, 0, 0, 0]);
    conn.write_all(&frame::encode(&swipe).unwrap())
        .await
        .unwrap();
    // BB 结束 → BC 确认。
    let started = china_offset()
        .with_ymd_and_hms(2024, 8, 29, 15, 0, 0)
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let ended = started + chrono::Duration::seconds(300);
    let end = device::build_charge_end(&device::ChargeEndReport {
        port: 2,
        order_bcd: [0; 8],
        started_at: started,
        ended_at: ended,
        charged_mwh: 100_000,
        power_deci_watts: 1500,
        stop_reason: 2,
        consumer_type: 2,
    })
    .unwrap();
    conn.write_all(&frame::encode(&end).unwrap()).await.unwrap();
    let bc = read_frame(&mut conn).await;
    assert_eq!(bc.command, 0xBC);
    assert_eq!(bc.data, vec![0, 2]);

    // 事件核对。
    let events = sink.events.lock().unwrap();
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&Some(chargepilot_protocol::EventType::Heartbeat)));
    assert!(kinds.contains(&Some(chargepilot_protocol::EventType::CardSwipe)));
    assert!(kinds.contains(&Some(chargepilot_protocol::EventType::ChargeEnd)));
    drop(events);

    // 主动断开 → 审计记录 close_reason=device_closed。
    drop(conn);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let records = recorder.records.lock().unwrap();
    assert_eq!(records.len(), 1, "一条会话审计: {records:?}");
    assert_eq!(records[0].close_reason, "device_closed");
    assert_eq!(records[0].device_id, "5348240514082652");
    assert!(records[0].frames_in >= 4, "注册+2心跳+刷卡+结束");
    assert!(records[0].frames_out >= 5, "A1+A6+A5×2+BC");
}

#[tokio::test]
async fn same_device_second_connection_replaces_first() {
    let sink = Arc::new(MemSink::default());
    let recorder = Arc::new(MemRecorder::default());
    let registry = Arc::new(chargepilot_gateway_runtime::SessionRegistry::new());
    let addr = serve_one(sink.clone(), recorder.clone(), Some(registry.clone())).await;
    let reg = device::build_registration(&identity()).unwrap();

    // 第一条连接完成注册。
    let mut first = tokio::net::TcpStream::connect(addr).await.unwrap();
    first
        .write_all(&frame::encode(&reg).unwrap())
        .await
        .unwrap();
    let _a1 = read_frame(&mut first).await;
    let _a6 = read_frame(&mut first).await;
    assert!(
        registry.get("5348240514082652").await.is_some(),
        "首连接已登记"
    );

    // 第二条连接同设备注册：互踢。
    let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();
    second
        .write_all(&frame::encode(&reg).unwrap())
        .await
        .unwrap();
    let _a1 = read_frame(&mut second).await;
    let _a6 = read_frame(&mut second).await;
    // 旧连接被关闭：读返回错误或 EOF。
    let mut buf = [0u8; 16];
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), first.read(&mut buf))
        .await
        .expect("互踢超时");
    assert!(result.is_err() || result.unwrap() == 0, "旧连接应被关闭");

    // 第一条连接的审计 close_reason=replaced。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let records = recorder.records.lock().unwrap();
    assert!(
        records.iter().any(|r| r.close_reason == "replaced"),
        "旧连接标记 replaced: {records:?}"
    );
}
