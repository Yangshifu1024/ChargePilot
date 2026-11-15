//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 批 3 真库集成测试：设备注册（未导入拒绝）、心跳遥测批量落库、
//! 端口上报状态、业务事件 device_event+domain_event 同事务落库、
//! 会话审计行。串行锁内自建数据、尾部清理。

use std::sync::Arc;

use chargepilot_gateway_runtime::{GatewaySink, SnapshotStore, TelemetryBuffer};
use chargepilot_protocol::{Event, EventType, Registration, SessionRecord, SessionRecorder as _};

#[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn cleanup(db: &sqlx::MySqlPool, device_id: &str) {
    let _ = sqlx::query("DELETE FROM `device_session` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device_event` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query(
        "DELETE FROM `domain_event` WHERE `source` = 'gateway' AND `payload_json`->>'$.device' = ?",
    )
    .bind(device_id)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `telemetry` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `telemetry_aggregate_15min` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device_meta` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn register_heartbeat_events_and_flush() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 4).await.unwrap();
    let device_id = format!("sinktest{}", rand_suffix());
    cleanup(&db, &device_id).await;

    // 导入设备事实（device_meta + device + 两端口）。
    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `port_count`) VALUES (?, 1, 2)")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    for port in 1..=2u8 {
        sqlx::query(
            "INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`) VALUES (?, ?, ?)",
        )
        .bind(&device_id)
        .bind(port)
        .bind(format!("{device_id}-port-{port}"))
        .execute(&db)
        .await
        .unwrap();
    }

    let snapshots = Arc::new(SnapshotStore::new());
    let telemetry = Arc::new(TelemetryBuffer::new());
    let sink = GatewaySink::new(db.clone(), snapshots.clone(), telemetry.clone());

    // 未导入设备注册拒绝。
    let stranger = Registration {
        protocol: "dc589",
        device_id: format!("stranger{}", rand_suffix()),
        hardware_version: "X".into(),
        software_version: "1".into(),
        received_at: now,
    };
    assert!(
        chargepilot_protocol::Sink::register(&sink, stranger)
            .await
            .is_err(),
        "未导入设备应拒绝"
    );

    // 已导入设备注册成功 + device 行刷新。
    let registration = Registration {
        protocol: "dc589",
        device_id: device_id.clone(),
        hardware_version: "SH10HP01".into(),
        software_version: "SH10SP02:107".into(),
        received_at: now,
    };
    chargepilot_protocol::Sink::register(&sink, registration)
        .await
        .unwrap();
    let firmware: String =
        sqlx::query_scalar("SELECT `firmware_version` FROM `device` WHERE `device_id` = ?")
            .bind(&device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(firmware, "SH10SP02:107");

    // 心跳事件：快照 + 端口状态 + 遥测缓冲。
    let heartbeat = Event {
        device_id: device_id.clone(),
        kind: Some(EventType::Heartbeat),
        received_at: Some(now),
        signal: 21,
        device_status: 0,
        voltage_v: 220,
        temperature_c: 25,
        port_states: vec![0, 1],
        charging_ports: vec![chargepilot_protocol::PortTelemetry {
            port: 2,
            remaining_secs: 600,
            charged_seconds: 100,
            charged_mwh: 50_000,
            power_deci_watts: 1200,
            ..Default::default()
        }],
        ..Event::default()
    };
    chargepilot_protocol::Sink::record(&sink, heartbeat.clone())
        .await
        .unwrap();
    let snap = snapshots
        .get_or_load(&device_id, |_| async { None })
        .await
        .unwrap();
    assert!(snap.online);
    assert_eq!(snap.port(2).unwrap().power_deci_watts, 1200);
    assert_eq!(snap.port(1).unwrap().reported_status, Some(0));
    assert_eq!(telemetry.received(), 4, "电压+温度+功率+电量");
    // 心跳落库事实：signal_strength 列是 tinyint unsigned，回归曾因解码方
    // Option<i64> 断裂（仅生产非 NULL 行触发，本地 NULL 数据测不出来）。
    // 按 unsigned 单字节解码断言锁定列类型与解码方一致（CODING_STANDARDS
    // 规则①②的产生端实例）。
    let (sig, has_hb, has_seen, has_sig_at): (u8, bool, bool, bool) = sqlx::query_as(
        "SELECT `signal_strength`, `last_heartbeat_at` IS NOT NULL, \
         `last_seen_at` IS NOT NULL, `signal_at` IS NOT NULL \
         FROM `device` WHERE `device_id` = ?",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(sig, 21, "心跳信号 21 应落库并按 tinyint unsigned 解码");
    assert!(
        has_hb && has_seen && has_sig_at,
        "心跳时刻、在线时刻与信号时刻应非 NULL"
    );

    // 业务事件落库（刷卡）。
    let swipe = Event {
        device_id: device_id.clone(),
        kind: Some(EventType::CardSwipe),
        port: 2,
        card_number: 17564161,
        received_at: Some(now),
        event_id: "evt-1".into(),
        ..Event::default()
    };
    chargepilot_protocol::Sink::record(&sink, swipe)
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `device_event` WHERE `device_id` = ?")
            .bind(&device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let processed: Option<i64> = sqlx::query_scalar(
        "SELECT `processed_at` IS NOT NULL FROM `device_event` WHERE `device_id` = ?",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(processed, Some(1), "单体同步处理立即标记");
    let domain_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `domain_event` WHERE `source` = 'gateway' AND `event_id` LIKE 'device-event-%'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(domain_count >= 1, "领域事件同事务入队");

    // 遥测 flush：明细 + 两级聚合。
    let samples = telemetry.drain();
    assert_eq!(samples.len(), 4);
    chargepilot_gateway_runtime::telemetry::flush(&db, &samples)
        .await
        .unwrap();
    let meter_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `telemetry` WHERE `device_id` = ? AND `metric` = 'power_w'",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(meter_rows >= 1, "功率明细已落库");
    let agg_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `telemetry_aggregate_15min` WHERE `device_id` = ?",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(agg_rows >= 1, "15 分钟聚合已落库");

    // 会话审计行。
    let record = SessionRecord {
        session_id: format!("sess-{}", rand_suffix()),
        device_id: device_id.clone(),
        remote_addr: "127.0.0.1:5555".into(),
        started_at: Some(now),
        last_active: Some(now),
        ended_at: Some(now),
        close_reason: "device_closed".into(),
        bytes_in: 100,
        bytes_out: 200,
        frames_in: 5,
        frames_out: 6,
        unknown_commands: vec![0xC7],
    };
    sink.record_session(record).await.unwrap();
    let close: String =
        sqlx::query_scalar("SELECT `close_reason` FROM `device_session` WHERE `device_id` = ?")
            .bind(&device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(close, "device_closed");

    cleanup(&db, &device_id).await;
}
