//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 06 批 5 主缝闭环测试（spec：模拟器单台闭环）：
//! 设备 TCP 注册 → 支付订单 → 启动命令经会话下发（B7 帧核对）→
//! B8 回执推进订单 charging → BB 结束回执推进 completed + 计费任务 →
//! 停机信号收尾。管理面走真库，设备面走真实 TCP。

use std::sync::Arc;

use chargepilot_gateway_runtime::{
    DeviceEventDispatcher, GatewayCommandSink, GatewaySink, SessionRegistry, SnapshotStore,
    TelemetryBuffer,
};
use chargepilot_mod_charging::sink::CommandSink as _;
use chargepilot_platform::ids::SegmentAllocator;
use chargepilot_protocol_dc589::adapter::TcpAdapter;
use chargepilot_protocol_dc589::frame::{self, Frame};
use chargepilot_protocol_dc589::{commands as cmd, device};
use serde_json::json;
use tokio::io::AsyncWriteExt as _;

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

async fn read_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Frame {
    tokio::time::timeout(std::time::Duration::from_secs(5), frame::read_frame(reader))
        .await
        .expect("读帧超时")
        .expect("读帧失败")
}

async fn cleanup(
    db: &sqlx::MySqlPool,
    device_id: &str,
    charge_order_id: u64,
    payment_id: u64,
    rule_id: u64,
) {
    let _ = sqlx::query("DELETE FROM `charge_billing_job` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_end_receipt` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_stop_command` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_command` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `active_port_charge` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_payment_intent` WHERE `payment_order_id` = ?")
        .bind(payment_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
        .bind(payment_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device_event` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `domain_event` WHERE `source` = 'gateway' AND `event_id` LIKE 'device-event-%'")
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `telemetry` WHERE `device_id` = ?")
        .bind(device_id)
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

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn single_device_closed_loop() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 6).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();

    // 板号必须 16 位十进制（8 字节 BCD）；其余表内标识用 loop- 前缀。
    let device_id = format!(
        "5348{:012}",
        rand::Rng::random_range(&mut rand::rng(), 0u64..999_999_999_999)
    );
    let port_code = format!("pc-{}", rand_suffix());
    let rule_id = alloc.next_id().await.unwrap();
    // 设备导入事实（device_meta/device/device_port + 计费规则）。
    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    let vendor_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `status`) VALUES (?, ?, '闭环测试厂商', 'dc589', 'enabled')")
        .bind(vendor_id as i64)
        .bind(format!("v{}", rand_suffix()))
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `port_count`, `status`) VALUES (?, ?, 2, 'enabled')")
        .bind(&device_id)
        .bind(vendor_id as i64)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 1, ?, 'idle')")
        .bind(&device_id)
        .bind(&port_code)
        .execute(&db)
        .await
        .unwrap();
    let scheme = json!({
        "name": "闭环测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`) VALUES (?, '闭环规则', NULL, ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64)
        .bind(&device_id)
        .bind(json!({"scheme": scheme}).to_string())
        .execute(&db)
        .await
        .unwrap();

    // 支付链（阶段04 入口）：模拟确认 → paid 订单 + 快照冻结。
    let user_id = alloc.next_id().await.unwrap();
    let payment_id = alloc.next_id().await.unwrap();
    let merchant_order_no = format!("P{payment_id}");
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, \
         `pay_method`, `total_cents`, `status`, `created_month`) \
         VALUES (?, ?, 'charge', 0, ?, 'wechat', 100, 'initiated', ?)",
    )
    .bind(payment_id as i64)
    .bind(&merchant_order_no)
    .bind(user_id as i64)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&db)
    .await
    .unwrap();
    let snapshot = json!({
        "rule": {"rule_id": rule_id, "station_id": 0, "version": 1,
                 "spec": {"mode": "server_energy",
                     "electric": {"basis": "energy", "periods": [
                         {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]}}},
        "estimate": {"estimated_kwh": "0.000", "estimated_minutes": 600,
                     "electric_cents": 0, "service_cents": 0, "total_cents": 100,
                     "charge_mode": 4, "charge_quantity": 600},
        "offer": {"id": rule_id * 100 + 1, "package_id": 1, "name": "1元",
                  "mode": "amount", "price_cents": 100},
        "computed_at_unix_micros": chrono::Utc::now().timestamp_micros()
    });
    sqlx::query(
        "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `merchant_order_no`, \
         `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, `port_code`, `station_id`, \
         `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, \
         `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, \
         `charge_quantity`, `status`, `expires_at`, `offer_id`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, 0, ?, 1, CAST(? AS JSON), '0.000', \
         600, 0, 0, 100, 4, 600, 'initiated', ?, ?)",
    )
    .bind(format!("loop-{}", rand_suffix()))
    .bind(format!("hint-{}", rand_suffix()))
    .bind(&merchant_order_no)
    .bind(payment_id as i64)
    .bind(user_id as i64)
    .bind(format!("sim-{user_id}"))
    .bind(&device_id)
    .bind(&port_code)
    .bind(rule_id as i64)
    .bind(serde_json::to_string(&snapshot).unwrap())
    .bind(chrono::Utc::now() + chrono::Duration::minutes(5))
    .bind((rule_id * 100 + 1) as i64)
    .execute(&db)
    .await
    .unwrap();
    let result = chargepilot_mod_payment::apply::apply_payment(
        &db,
        &chargepilot_mod_payment::apply::VerifiedPayment {
            merchant_order_no: merchant_order_no.clone(),
            transaction_id: format!("LOOP{merchant_order_no}"),
            open_id: format!("sim-{user_id}"),
            paid_cents: 100,
            paid_at_unix_micros: chrono::Utc::now().timestamp_micros(),
        },
    )
    .await
    .expect("支付成功事务应成功");
    let charge_order_id = result.charge_order_id;
    let order_no = result.charge_order_no.clone();

    // 网关装配：listener + sink + registry + 命令通道。
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let snapshots = Arc::new(SnapshotStore::new());
    let telemetry = Arc::new(TelemetryBuffer::new());
    let sink = Arc::new(GatewaySink::new(
        db.clone(),
        snapshots.clone(),
        telemetry.clone(),
    ));
    let registry = Arc::new(SessionRegistry::new());
    let listener = chargepilot_platform::lifecycle::TaskManager::new();
    drop(listener);
    let addr = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let device_listener = chargepilot_gateway_runtime::DeviceListener::new(
        Arc::new(TcpAdapter::new()),
        sink.clone(),
        sink.clone(),
        registry.clone(),
    );
    let accept_token =
        chargepilot_platform::lifecycle::CancelToken::from_watch(shutdown_rx.clone());
    let _accept_task = tokio::spawn({
        let device_listener = device_listener;
        async move { device_listener.run(addr, accept_token).await }
    });
    let command_sink = Arc::new(GatewayCommandSink::new(registry.clone()));
    let dispatcher = DeviceEventDispatcher {
        pool: db.clone(),
        allocator: alloc.clone(),
        registry: registry.clone(),
    };

    // 设备连接：等待监听就绪（spawn 的 bind 竞态）后注册。
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
    let identity = device::DeviceIdentity {
        board_id: device_id.clone(),
        hardware_version: "SH10HP01".into(),
        software_id: "SH10SP02".into(),
        software_version: 107,
        module_id: "0863121077410587".into(),
        sim: "898604041018C0013999".into(),
        signal: 20,
    };
    let reg_frame =
        device::build_registration(&identity).unwrap_or_else(|e| panic!("注册帧编码失败: {e:?}"));
    dev.write_all(&frame::encode(&reg_frame).unwrap())
        .await
        .unwrap();
    let a1 = read_frame(&mut dev).await;
    assert_eq!(a1.command, 0xA1);
    let a6 = read_frame(&mut dev).await;
    assert_eq!(a6.command, 0xA6);

    // 种启动命令行（StartDispatcher 产物：status=sent）；直接构造绕过
    // 支付事件链，命令内容与订单一致。
    let command_id = format!("cmd-{}", rand_suffix());
    sqlx::query(
        "INSERT INTO `charge_command` (`command_id`, `stop_command_id`, `charge_order_id`,          `payment_order_id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`,          `port_id`, `owns_port`, `status`, `sent_at`, `charge_mode`, `quantity`,          `consumer_type`) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, 1, 'sent', UTC_TIMESTAMP(3), 4, 600, 2)",
    )
    .bind(&command_id)
    .bind(format!("stop-{}", rand_suffix()))
    .bind(charge_order_id as i64)
    .bind(payment_id as i64)
    .bind(&order_no)
    .bind(user_id as i64)
    .bind(&device_id)
    .bind(&port_code)
    .bind(charge_order_id as i64)
    .execute(&db)
    .await
    .unwrap();

    // 启动命令：设备侧收到 B7（端口、订单 BCD、平台计费、600 分钟）。
    command_sink
        .send_start(&chargepilot_mod_charging::sink::StartCommand {
            command_id: command_id.clone(),
            charge_order_id,
            device_id: device_id.clone(),
            port_no: 1,
            charge_mode: 4,
            quantity: 600,
        })
        .await
        .expect("启动命令应下发成功");
    let b7 = read_frame(&mut dev).await;
    assert_eq!(b7.command, 0xB7);
    let (port, order_bcd, mode, quantity, consumer, _, _) =
        device::parse_start_command(&b7.data).unwrap();
    assert_eq!(port, 1);
    assert_eq!(mode, cmd::ChargeMode::PlatformBilling);
    assert_eq!(quantity, 600);
    assert_eq!(consumer, 2);
    // 订单号 BCD = 订单 ID 十六位补零。
    let expected_bcd =
        chargepilot_gateway_runtime::command_sink::numeric_order_bcd(charge_order_id).unwrap();
    assert_eq!(order_bcd, expected_bcd);
    // B7 后心跳切 15 秒。
    let a6 = read_frame(&mut dev).await;
    assert_eq!(a6.command, 0xA6);
    assert_eq!(
        device::parse_heartbeat_interval(&a6.data).unwrap(),
        (15, true)
    );

    // 设备回 B8 成功 → 事件落库 → 处理器推进订单 charging。
    let b8 = device::build_command_result(0xB8, 0, 1).unwrap();
    dev.write_all(&frame::encode(&b8).unwrap()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let event_id: String = sqlx::query_scalar(
        "SELECT CAST(`event_key` AS CHAR) FROM `device_event` WHERE `device_id` = ? AND `event_type` = 'StartResult' \
         ORDER BY `id` DESC LIMIT 1",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT `event_json` FROM `device_event` WHERE `event_key` = ?")
            .bind(&event_id)
            .fetch_one(&db)
            .await
            .unwrap();
    dispatcher.handle(&payload).await.unwrap();
    let order_status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(order_status, "charging", "B8 成功后订单进入充电中");
    let cmd_status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `charge_command` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(cmd_status, "acked");

    // 种停止命令行（用户停止/自动停机流程产物：status=sent）。
    let stop_command_id = format!("stop-{}", rand_suffix());
    sqlx::query(
        "INSERT INTO `charge_stop_command` (`command_id`, `start_command_id`, `charge_order_id`, \
         `order_no`, `user_id`, `device_id`, `port_no`, `port_id`, `status`, `sent_at`) \
         VALUES (?, ?, ?, ?, ?, ?, 1, ?, 'sent', UTC_TIMESTAMP(3))",
    )
    .bind(&stop_command_id)
    .bind(&command_id)
    .bind(charge_order_id as i64)
    .bind(&order_no)
    .bind(user_id as i64)
    .bind(&device_id)
    .bind(charge_order_id as i64)
    .execute(&db)
    .await
    .unwrap();

    // BB 结束：已充 10 分钟 0.5 kWh → 结束回执 → 订单 completed + 计费任务。
    // 设备侧时间取当前时刻加 2 秒余量：帧时间经 BCD 截断到秒，必须仍不早于
    // 订单 started_at（B8 处理时刻的微秒精度时间戳）。
    let ended = chrono::Utc::now() + chrono::Duration::seconds(2);
    let started = ended - chrono::Duration::minutes(10);
    let bb = device::build_charge_end(&device::ChargeEndReport {
        port: 1,
        order_bcd: expected_bcd,
        started_at: started,
        ended_at: ended,
        charged_mwh: 500_000,
        power_deci_watts: 1500,
        stop_reason: 7,
        consumer_type: 2,
    })
    .unwrap();
    dev.write_all(&frame::encode(&bb).unwrap()).await.unwrap();
    let _bc = read_frame(&mut dev).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT `event_json` FROM `device_event` WHERE `device_id` = ? AND `event_type` = 'ChargeEnd' \
         ORDER BY `id` DESC LIMIT 1",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    dispatcher.handle(&payload).await.unwrap();
    let order_status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(order_status, "completed", "结束回执推进订单完成");
    let billing_jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(billing_jobs >= 1, "结束回执提交计费任务");

    // 停机：取消信号 → listener 退出；遥测缓冲冲刷。
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    drop(dev);

    cleanup(&db, &device_id, charge_order_id, payment_id, rule_id).await;
}
