//! 阶段 06 设备事件链真库测试（TEST_MYSQL_URL 门控）：
//! B6 刷卡（受理授权/余额不足拒绝/重放幂等）、C0 告警（生成/重放合并/
//! 心跳恢复）、B4 本地启动补单（订单落地/B5 确认/卡无效拒绝）。
//! 帧断言经 FakeSession 捕获（命令通道）与真实 TCP 会话（B5/B7）。

use std::sync::{Arc, Mutex};

use chargepilot_gateway_runtime::{
    DeviceEventDispatcher, DeviceListener, GatewayCommandSink, GatewaySink, SessionRegistry,
    SnapshotStore, TelemetryBuffer,
};
use chargepilot_platform::ids::SegmentAllocator;
use chargepilot_protocol::{Command, DeviceSession, SessionRouter as _};
use chargepilot_protocol_dc589::adapter::TcpAdapter;
use chargepilot_protocol_dc589::frame::{self, Frame};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

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

fn rand_card() -> u32 {
    // 十进制非零 u32 卡号（域层 valid_card_no 规则）。
    rand::Rng::random_range(&mut rand::rng(), 10_000_000u32..99_999_999)
}

async fn read_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Frame {
    tokio::time::timeout(std::time::Duration::from_secs(5), frame::read_frame(reader))
        .await
        .expect("读帧超时")
        .expect("读帧失败")
}

/// 捕获下发命令的假会话（命令通道断言用，不发真实 IO）。
#[derive(Default)]
struct FakeSession {
    commands: Mutex<Vec<Command>>,
}

impl FakeSession {
    fn captured(&self) -> Vec<Command> {
        self.commands.lock().expect("命令锁").clone()
    }
}

impl DeviceSession for FakeSession {
    fn send(&self, command: Command) -> chargepilot_protocol::BoxFut<anyhow::Result<()>> {
        self.commands.lock().expect("命令锁").push(command);
        Box::pin(async { Ok(()) })
    }
    fn close(&self) -> chargepilot_protocol::BoxFut<()> {
        Box::pin(async {})
    }
}

/// 刷卡链测试上下文：站点 + 设备 + 端口 + 方案 + 用户 + 钱包 + 在线卡。
struct CardCtx {
    station_id: u64,
    device_id: String,
    port_code: String,
    rule_id: u64,
    user_id: u64,
    card_no: u32,
    wallet_balance: i64,
}

/// 种刷卡链基础数据（活动站点 + 设备事实 + 活动方案 + active 用户 + 富余钱包 + 在线卡）。
async fn setup_card_ctx(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> CardCtx {
    let device_id = format!(
        "5348{:012}",
        rand::Rng::random_range(&mut rand::rng(), 0u64..999_999_999_999)
    );
    let port_code = format!("pc-{}", rand_suffix());
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    let card_no = rand_card();
    let wallet_balance = 10_000; // 100 元
    // 刷卡准入经 active_device_rule 读活动站点下的方案（JOIN station）。
    sqlx::query(
        "INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) \
         VALUES (?, '刷卡链站点', 116.39712800, 39.91652700, 'active')",
    )
    .bind(station_id as i64)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO `device` (`device_id`, `vendor_id`, `station_id`, `port_count`, `status`) \
         VALUES (?, 1, ?, 2, 'enabled')",
    )
    .bind(&device_id)
    .bind(station_id as i64)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) \
         VALUES (?, 1, ?, 'idle')",
    )
    .bind(&device_id)
    .bind(&port_code)
    .execute(db)
    .await
    .unwrap();
    // 刷卡方案：在线卡套餐须为时长模式且被 card.package_id 引用（域层校验）。
    let scheme = json!({
        "name": "刷卡链测试方案", "remark": "",
        "packages": [{"id": 1, "name": "1元600分", "mode": "duration",
                      "minutes": 600, "price_cents": 100, "kwh": 0}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600,
                   "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    sqlx::query(
        "INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, \
         `status`, `spec_json`) VALUES (?, '刷卡链规则', ?, ?, 1, 'active', CAST(? AS JSON))",
    )
    .bind(rule_id as i64)
    .bind(station_id as i64)
    .bind(&device_id)
    .bind(json!({"scheme": scheme}).to_string())
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `status`) VALUES (?, ?, 'active')")
        .bind(user_id as i64)
        .bind(format!("sim-{user_id}").into_bytes())
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO `wallet_account` (`user_id`, `balance_cents`, `frozen_cents`, `status`) \
         VALUES (?, ?, 0, 'active')",
    )
    .bind(user_id as i64)
    .bind(wallet_balance)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `online_card` (`card_no`, `user_id`, `status`) VALUES (?, ?, 'active')",
    )
    .bind(card_no.to_string())
    .bind(user_id as i64)
    .execute(db)
    .await
    .unwrap();
    CardCtx {
        station_id,
        device_id,
        port_code,
        rule_id,
        user_id,
        card_no,
        wallet_balance,
    }
}

/// 清理刷卡链测试数据（子表 → 主表顺序）。
async fn cleanup_card_ctx(db: &sqlx::MySqlPool, ctx: &CardCtx) {
    let _ = sqlx::query(
        "DELETE t FROM `wallet_txn` t JOIN `wallet_account` w ON w.`id` = t.`wallet_account_id` \
         WHERE w.`user_id` = ?",
    )
    .bind(ctx.user_id as i64)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `wallet_account` WHERE `user_id` = ?")
        .bind(ctx.user_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
        .bind(ctx.user_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `online_card` WHERE `user_id` = ?")
        .bind(ctx.user_id as i64)
        .execute(db)
        .await;
    cleanup_device(db, &ctx.device_id, ctx.rule_id).await;
    cleanup_station(db, ctx.station_id).await;
}

/// 清理设备侧数据（事件、告警、订单、命令与会话审计）。
async fn cleanup_device(db: &sqlx::MySqlPool, device_id: &str, rule_id: u64) {
    let _ = sqlx::query("DELETE FROM `charge_billing_job` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query(
        "DELETE o FROM `charge_order` o \
         JOIN `charge_order_pricing` p ON p.`charge_order_id` = o.`id` WHERE o.`device_id` = ?",
    )
    .bind(device_id)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `charge_order_pricing` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_order` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query(
        "DELETE i FROM `charge_payment_intent` i LEFT JOIN `charge_order` o \
         ON o.`id` = i.`charge_order_id` WHERE o.`id` IS NULL AND i.`device_id` = ?",
    )
    .bind(device_id)
    .execute(db)
    .await;
    let _ = sqlx::query(
        "DELETE p FROM `payment_order` p LEFT JOIN `charge_order` o \
         ON o.`payment_order_id` = p.`id` WHERE o.`id` IS NULL AND p.`order_no` LIKE 'P%'",
    )
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `charge_command` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_stop_command` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `active_port_charge` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `card_operation` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query(
        "DELETE FROM `card_charge` WHERE `port_code` IN \
         (SELECT `port_code` FROM `device_port` WHERE `device_id` = ?)",
    )
    .bind(device_id)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `alert_event` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device_event` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query(
        "DELETE FROM `domain_event` WHERE `source` = 'gateway' \
         AND `event_id` LIKE 'device-event-%'",
    )
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `device_session` WHERE `device_id` = ?")
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
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(rule_id as i64)
        .execute(db)
        .await;
}

/// 清理站点（刷卡链夹具；放在设备清理后调用避免关联残留）。
async fn cleanup_station(db: &sqlx::MySqlPool, station_id: u64) {
    let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
        .bind(station_id as i64)
        .execute(db)
        .await;
}

/// 与 GatewaySink::persist_event 同构的事件载荷。
fn device_payload(
    kind: &str,
    device_id: &str,
    event_key: &str,
    event_id: &str,
    port: u8,
    fields: Value,
) -> Value {
    let mut payload = json!({
        "event_id": event_id,
        "event_key": event_key,
        "device_id": device_id,
        "type": kind,
        "port": port,
        "result_code": 0,
        "fault_code": 0,
        "energy_wh": 0,
        "power_deci_watts": 0,
        "stop_reason": 0,
        "consumer_type": 0,
        "card_number": 0,
        "order_number": "",
        "charged_seconds": 0,
        "received_at": chrono::Utc::now().to_rfc3339(),
        "started_at": Value::Null,
        "ended_at": Value::Null,
        "amount_cents": 0,
        "signal": 0,
    });
    for (k, v) in fields.as_object().into_iter().flatten() {
        payload[k.as_str()] = v.clone();
    }
    payload
}

/// 装配设备事件派发器与假会话（挂在指定设备号上供帧捕获）。
async fn build_dispatcher(
    db: sqlx::MySqlPool,
    registry: &Arc<SessionRegistry>,
    fake: Arc<FakeSession>,
    device_id: &str,
) -> DeviceEventDispatcher {
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let _attached = registry.attach(
        device_id,
        fake.clone() as Arc<dyn DeviceSession>,
        Arc::new(|| {}),
    );
    DeviceEventDispatcher {
        pool: db,
        allocator: alloc,
        registry: registry.clone(),
    }
}

/// B4 帧载荷（对齐模拟器 portReport 布局）：端口、状态、订单号、民用时间、
/// 剩余分钟、消费类型 [23]、卡号 [32..36]。
fn b4_data(port: u8, consumer: u8, card: u32, started: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
    use chrono::{Datelike as _, Timelike as _};
    let mut d = vec![0u8; 36];
    d[0] = port;
    d[1] = 1; // 状态充电
    let local = started.with_timezone(&chargepilot_protocol_dc589::china_offset());
    let civil = [
        i32::from(local.year() % 100),
        local.month() as i32,
        local.day() as i32,
        local.hour() as i32,
        local.minute() as i32,
        local.second() as i32,
    ];
    for (i, n) in civil.iter().enumerate() {
        d[10 + i] = ((n / 10) << 4 | (n % 10)) as u8;
    }
    d[16..18].copy_from_slice(&100u16.to_le_bytes()); // 剩余 100 分钟
    d[19] = 0; // 按时间计费
    d[23] = consumer;
    d[32..36].copy_from_slice(&card.to_le_bytes());
    d
}

/// 从 device_event 取最新指定类型事件的载荷（与闭环测试惯例一致）。
async fn latest_payload(db: &sqlx::MySqlPool, device_id: &str, event_type: &str) -> Value {
    sqlx::query_scalar(
        "SELECT `event_json` FROM `device_event` \
         WHERE `device_id` = ? AND `event_type` = ? ORDER BY `id` DESC LIMIT 1",
    )
    .bind(device_id)
    .bind(event_type)
    .fetch_one(db)
    .await
    .unwrap()
}

/// 刷卡链：B6 事件受理 → 订单/卡扣/会话落地 → 授权（B7）经命令通道下发。
#[tokio::test]
async fn card_swipe_authorizes_with_start_command() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return;
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let ctx = setup_card_ctx(&db, &alloc).await;
    let registry = Arc::new(SessionRegistry::new());
    let fake = Arc::new(FakeSession::default());
    let dispatcher = build_dispatcher(db.clone(), &registry, fake.clone(), &ctx.device_id).await;
    // 启动命令通道（StartDispatcher 的真实实现）。
    let start_dispatcher = chargepilot_mod_charging::StartDispatcher {
        pool: db.clone(),
        allocator: alloc.clone(),
        sink: Arc::new(GatewayCommandSink::new(registry.clone())),
        simulation: false,
    };

    let received = chrono::Utc::now();
    let event_id = format!("swipe-{}", rand_suffix());
    let payload = device_payload(
        "CardSwipe",
        &ctx.device_id,
        &format!("key-{}", rand_suffix()),
        &event_id,
        1,
        json!({"card_number": ctx.card_no, "received_at": received.to_rfc3339()}),
    );
    dispatcher.handle(&payload).await.expect("刷卡受理应成功");

    // 订单落地：paid 订单 + 刷卡会话 + 操作记录（confirming）。
    let order_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(order_count, 1, "刷卡首购补建订单");
    let (order_id, order_status): (u64, String) = sqlx::query_as(
        "SELECT `id`, CAST(`status` AS CHAR) FROM `charge_order` WHERE `device_id` = ?",
    )
    .bind(&ctx.device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(order_status, "paid");
    let operation: (String, i64, u16) = sqlx::query_as(
        "SELECT CAST(`status` AS CHAR), `price_cents`, `minutes` FROM `card_operation` \
         WHERE `device_id` = ? AND `event_id` = ?",
    )
    .bind(&ctx.device_id)
    .bind(&event_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(operation, ("confirming".into(), 100, 600), "操作记录与套餐");
    let wallet_out: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `wallet_txn` t JOIN `wallet_account` w \
         ON w.`id` = t.`wallet_account_id` WHERE w.`user_id` = ? AND t.`direction` = 'out'",
    )
    .bind(ctx.user_id as i64)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(wallet_out, 1, "钱包扣款一笔");
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(ctx.user_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(balance, ctx.wallet_balance - 100, "扣款后余额");

    // 受理不发卡回复（旧版语义）：此刻无任何命令下发。
    assert!(fake.captured().is_empty(), "受理路径不应下发卡回复帧");

    // 出站 charge_payment_confirmed → StartDispatcher → B7 授权命令。
    let confirmed: String = sqlx::query_scalar(
        "SELECT CAST(`payload_json` AS CHAR) FROM `domain_event` \
         WHERE `kind` = 'charge_payment_confirmed' ORDER BY `id` DESC LIMIT 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let confirmed_payload: Value = serde_json::from_str(&confirmed).unwrap();
    start_dispatcher
        .handle(&confirmed_payload)
        .await
        .expect("启动投递应成功");
    let command_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_command` WHERE `charge_order_id` = ?")
            .bind(order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(command_count, 1, "启动命令已登记");
    let captured = fake.captured();
    assert_eq!(captured.len(), 1, "授权帧经会话下发");
    assert_eq!(captured[0].kind, chargepilot_protocol::CommandKind::Start);
    assert_eq!(captured[0].port, 1);

    dispatcher_handle_twice_replay_check(&dispatcher, &payload, &db, &ctx).await;
    cleanup_card_ctx(&db, &ctx).await;
}

/// 刷卡重放幂等：同一事件两次处理只扣一次款、只建一单。
async fn dispatcher_handle_twice_replay_check(
    dispatcher: &DeviceEventDispatcher,
    payload: &Value,
    db: &sqlx::MySqlPool,
    ctx: &CardCtx,
) {
    dispatcher.handle(payload).await.expect("重放应幂等成功");
    let operations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `card_operation` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(operations, 1, "重放不新建操作记录");
    let orders: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(orders, 1, "重放不新建订单");
    let wallet_out: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `wallet_txn` t JOIN `wallet_account` w \
         ON w.`id` = t.`wallet_account_id` WHERE w.`user_id` = ? AND t.`direction` = 'out'",
    )
    .bind(ctx.user_id as i64)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(wallet_out, 1, "重放不重复扣款");
}

/// 刷卡拒绝：余额不足 → BD 拒绝帧（保留余额显示），不落订单不扣款。
#[tokio::test]
async fn card_swipe_insufficient_balance_denies() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return;
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let mut ctx = setup_card_ctx(&db, &alloc).await;
    // 钱包清零触发余额不足。
    sqlx::query("UPDATE `wallet_account` SET `balance_cents` = 0 WHERE `user_id` = ?")
        .bind(ctx.user_id as i64)
        .execute(&db)
        .await
        .unwrap();
    ctx.wallet_balance = 0;
    let registry = Arc::new(SessionRegistry::new());
    let fake = Arc::new(FakeSession::default());
    let dispatcher = build_dispatcher(db.clone(), &registry, fake.clone(), &ctx.device_id).await;

    let payload = device_payload(
        "CardSwipe",
        &ctx.device_id,
        &format!("key-{}", rand_suffix()),
        &format!("swipe-{}", rand_suffix()),
        1,
        json!({"card_number": ctx.card_no}),
    );
    dispatcher.handle(&payload).await.expect("拒绝路径应成功");

    let orders: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(orders, 0, "余额不足不建订单");
    let captured = fake.captured();
    assert_eq!(captured.len(), 1, "拒绝帧经会话下发");
    assert_eq!(
        captured[0].kind,
        chargepilot_protocol::CommandKind::CardDenied
    );
    assert!(
        !captured[0].card_invalid,
        "余额不足按可显示余额的拒绝（旧版 InsufficientBalance 语义）"
    );
    assert_eq!(captured[0].card_number, ctx.card_no);
    cleanup_card_ctx(&db, &ctx).await;
}

/// 告警链：故障事件生成告警，重放与合并升级不重复建行，恢复心跳置 auto_resolved。
#[tokio::test]
async fn fault_alert_records_and_recovers() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return;
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let ctx = setup_card_ctx(&db, &alloc).await;
    let registry = Arc::new(SessionRegistry::new());
    let fake = Arc::new(FakeSession::default());
    let dispatcher = build_dispatcher(db.clone(), &registry, fake.clone(), &ctx.device_id).await;
    let snapshots = Arc::new(SnapshotStore::new());
    let sink = Arc::new(GatewaySink::new(
        db.clone(),
        snapshots.clone(),
        Arc::new(TelemetryBuffer::new()),
    ));

    // 故障事件落库（事件键 = 幂等与恢复判定依据）。
    let fault_at = chrono::Utc::now();
    use chargepilot_protocol::{Event, EventType, Sink as _};
    sink.record(Event {
        device_id: ctx.device_id.clone(),
        kind: Some(EventType::Fault),
        port: 1,
        fault_code: 4,
        received_at: Some(fault_at),
        event_id: format!("fault-{}", rand_suffix()),
        ..Event::default()
    })
    .await
    .unwrap();
    let payload = latest_payload(&db, &ctx.device_id, "Fault").await;
    dispatcher.handle(&payload).await.expect("告警写入应成功");

    let (alert_id, metric, status, value): (u64, String, String, String) = sqlx::query_as(
        "SELECT `id`, `metric`, CAST(`status` AS CHAR), CAST(`value` AS CHAR) \
         FROM `alert_event` WHERE `device_id` = ?",
    )
    .bind(&ctx.device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(metric, "port_fault_1", "端口故障按端口号聚合");
    assert_eq!(status, "active");
    assert_eq!(
        value.split('.').next().unwrap_or(&value),
        "4",
        "告警观测值为协议故障码"
    );

    // 重放同一事件：不重复建行。
    dispatcher.handle(&payload).await.expect("重放应幂等");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM `alert_event` WHERE `device_id` = ?")
        .bind(&ctx.device_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 1, "重放不重复生成告警");

    // 新故障（不同事件键）→ 合并升级：仍一行 active，观测值更新。
    sink.record(Event {
        device_id: ctx.device_id.clone(),
        kind: Some(EventType::Fault),
        port: 1,
        fault_code: 5,
        received_at: Some(fault_at + chrono::Duration::seconds(1)),
        event_id: format!("fault-{}", rand_suffix()),
        ..Event::default()
    })
    .await
    .unwrap();
    let payload2 = latest_payload(&db, &ctx.device_id, "Fault").await;
    dispatcher.handle(&payload2).await.expect("合并升级应成功");
    let (count, value2, status2): (i64, String, String) = sqlx::query_as(
        "SELECT COUNT(*), CAST(MAX(`value`) AS CHAR), CAST(MIN(`status`) AS CHAR) \
         FROM `alert_event` WHERE `device_id` = ?",
    )
    .bind(&ctx.device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(count, 1, "同维度告警合并升级");
    assert_eq!(
        value2.split('.').next().unwrap_or(&value2),
        "5",
        "升级取最新故障码"
    );
    assert_eq!(status2, "active");

    // 恢复心跳（晚于最新故障、端口状态归零）→ auto_resolved。
    sink.record(Event {
        device_id: ctx.device_id.clone(),
        kind: Some(EventType::Heartbeat),
        received_at: Some(fault_at + chrono::Duration::seconds(10)),
        device_status: 0,
        port_states: vec![0, 0],
        ..Event::default()
    })
    .await
    .unwrap();
    let status3: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `alert_event` WHERE `id` = ?")
            .bind(alert_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status3, "auto_resolved", "恢复心跳消除告警");

    cleanup_card_ctx(&db, &ctx).await;
}

/// B4 本地启动：真实 TCP 会话下发 B5 确认，平台补单落地订单/卡扣；
/// 离线卡卡号不存在时按刷卡链下发 BD 拒绝。
#[tokio::test]
async fn local_start_confirms_and_compensates() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return;
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let ctx = setup_card_ctx(&db, &alloc).await;
    let snapshots = Arc::new(SnapshotStore::new());
    let telemetry = Arc::new(TelemetryBuffer::new());
    let sink = Arc::new(GatewaySink::new(
        db.clone(),
        snapshots.clone(),
        telemetry.clone(),
    ));
    let registry = Arc::new(SessionRegistry::new());
    let fake = Arc::new(FakeSession::default());
    let dispatcher = build_dispatcher(db.clone(), &registry, fake.clone(), &ctx.device_id).await;

    // 设备 TCP 接入：注册后发 B4（离线卡本地启动）。
    let addr = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let listener = DeviceListener::new(
        Arc::new(TcpAdapter::new()),
        sink.clone(),
        sink.clone(),
        registry.clone(),
    );
    let token = chargepilot_platform::lifecycle::CancelToken::from_watch(stop_rx);
    let _listener_task = tokio::spawn({
        let listener = listener;
        async move { listener.run(addr, token).await }
    });
    let mut dev = {
        let mut connected = None;
        for _ in 0..40 {
            if let Ok(c) = tokio::net::TcpStream::connect(addr).await {
                connected = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        connected.expect("监听未就绪")
    };
    let identity = chargepilot_protocol_dc589::device::DeviceIdentity {
        board_id: ctx.device_id.clone(),
        hardware_version: "SH10HP01".into(),
        software_id: "SH10SP02".into(),
        software_version: 107,
        module_id: "0863121077410587".into(),
        sim: "898604041018C0013999".into(),
        signal: 20,
    };
    let reg = chargepilot_protocol_dc589::device::build_registration(&identity)
        .unwrap_or_else(|e| panic!("注册帧编码失败: {e:?}"));
    dev.write_all(&frame::encode(&reg).unwrap()).await.unwrap();
    let _a1 = read_frame(&mut dev).await;
    let _a6 = read_frame(&mut dev).await;

    // B4 本地启动上报（消费类型 1 离线卡，卡号取平台在线卡）。
    let b4 = Frame::new(0xB4, [0; 6], b4_data(1, 1, ctx.card_no, chrono::Utc::now()));
    dev.write_all(&frame::encode(&b4).unwrap()).await.unwrap();
    let b5 = read_frame(&mut dev).await;
    assert_eq!(b5.command, 0xB5, "落库成功后下发 B5 确认");
    assert_eq!(&b5.data[..], &[0, 1], "B5：控制类型收到 + 端口号");
    // B4 载荷的卡号按协议偏移 [32..36] 解出（离线卡补单的准入输入）。
    let payload = latest_payload(&db, &ctx.device_id, "LocalStart").await;
    assert_eq!(
        payload["card_number"].as_u64(),
        Some(u64::from(ctx.card_no)),
        "B4 卡号按模拟器布局解出"
    );
    assert_eq!(
        payload["consumer_type"].as_u64(),
        Some(1),
        "消费类型在 [23]"
    );

    // 平台补单：复用刷卡准入事务（订单/卡扣/操作记录落地）。
    dispatcher.handle(&payload).await.expect("补单应成功");
    let order_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(order_count, 1, "本地启动补建订单");
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(ctx.user_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(balance, ctx.wallet_balance - 100, "平台侧卡扣一笔");
    let wallet_out: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `wallet_txn` t JOIN `wallet_account` w \
         ON w.`id` = t.`wallet_account_id` WHERE w.`user_id` = ? AND t.`direction` = 'out'",
    )
    .bind(ctx.user_id as i64)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(wallet_out, 1);

    let _ = _stop_tx.send(true);
    drop(dev);
    cleanup_card_ctx(&db, &ctx).await;
}

/// B4 卡无效路径：离线卡号未绑定 → BD 拒绝（invalid=true），不补单不扣款。
#[tokio::test]
async fn local_start_unknown_card_denies() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return;
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let ctx = setup_card_ctx(&db, &alloc).await;
    let registry = Arc::new(SessionRegistry::new());
    let fake = Arc::new(FakeSession::default());
    let dispatcher = build_dispatcher(db.clone(), &registry, fake.clone(), &ctx.device_id).await;

    // 消费类型 1（离线卡）但卡号未在平台绑定。
    let unknown_card = rand_card();
    let payload = device_payload(
        "LocalStart",
        &ctx.device_id,
        &format!("key-{}", rand_suffix()),
        &format!("local-{}", rand_suffix()),
        1,
        json!({"consumer_type": 1, "card_number": unknown_card}),
    );
    dispatcher
        .handle(&payload)
        .await
        .expect("卡无效拒绝路径应成功");
    let captured = fake.captured();
    assert_eq!(captured.len(), 1, "卡无效下发 BD 拒绝");
    assert_eq!(
        captured[0].kind,
        chargepilot_protocol::CommandKind::CardDenied
    );
    assert!(captured[0].card_invalid, "未知卡号按无效卡拒绝");
    assert_eq!(captured[0].card_number, unknown_card);
    let orders: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `device_id` = ?")
            .bind(&ctx.device_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(orders, 0, "卡无效不补单");
    cleanup_card_ctx(&db, &ctx).await;
}
