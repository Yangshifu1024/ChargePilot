//! 阶段 08 故障注入端到端（真库 + 真实时钟）：
//! 1. 回执迟到：启动命令超时 → 兜底扫描推进 failed + 退款登记 + 命令拒绝。
//! 2. 事件丢失/投递器故障：mark_failed 退避 → 重放成功 → 命令生成且标记 published。
//! 3. 进程重启：结束回执同内容重放幂等 → 状态收敛、计费任务不翻倍。
//! 4. 遥测缓冲满：明细丢弃计数增长、落库明细与聚合完好（分级语义固化）。

use chargepilot_events::EventDispatcher as _;
use chargepilot_gateway_runtime::TelemetryBuffer;
use chargepilot_platform::ids::SegmentAllocator;
use std::sync::Arc;

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 测试统一用户 ID（订单与意图同一用户）。
const USER_ID: u64 = 9001;

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 支付订单号分配辅助（intent 必填 payment_order_id，退款登记 JOIN 此表）。
async fn alloc_next_payment_id(alloc: &SegmentAllocator) -> u64 {
    alloc.next_id().await.unwrap()
}

async fn cleanup_order(
    db: &sqlx::MySqlPool,
    charge_order_id: u64,
    device_id: &str,
    event_id: &str,
) {
    let _ = sqlx::query("DELETE FROM `charge_billing_job` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_end_receipt` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_start_receipt` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `refund_record` WHERE `biz_id` = ?")
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
    let _ = sqlx::query("DELETE FROM `charge_payment_intent` WHERE `charge_order_id` = ?")
        .bind(charge_order_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
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
    let _ = sqlx::query("DELETE FROM `domain_event` WHERE `event_id` = ?")
        .bind(event_id)
        .execute(db)
        .await;
}

/// 场景 1：启动命令超时（回执迟到）——兜底扫描推进失败态并登记自动退款。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn slow_receipt_sweeper_fails_order_with_refund() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 4).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let device_id = format!("resil-{}", rand_suffix());
    let command_id = format!("cmd-{}", rand_suffix());

    // 设备事实 + paid 订单 + 超时启动命令（created_at 拨老 10 分钟）。
    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    let payment_id = alloc_next_payment_id(&alloc).await;
    sqlx::query(
        "INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, \
         `status`, `payment_order_id`, `created_month`) VALUES (?, ?, ?, ?, 1, ?, 'paid', ?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(format!("C{charge_order_id}"))
    .bind(USER_ID as i64)
    .bind(&device_id)
    .bind(format!("pc-{charge_order_id}"))
    .bind(payment_id as i64)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&db)
    .await
    .unwrap();
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now()).to_string();
    sqlx::query(&format!(
        "INSERT INTO `payment_order` (`id`, `order_no`, `user_id`, `biz_type`, `biz_id`, \
         `pay_method`, `total_cents`, `paid_cents`, `status`, `created_month`) \
         VALUES ({payment_id}, 'P{payment_id}', {USER_ID}, 'charge', {charge_order_id}, \
         'wechat', 100, 100, 'paid', '{month}')"
    ))
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `charge_order_id`, \
         `merchant_order_no`, `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, \
         `port_code`, `station_id`, `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, \
         `estimated_kwh`, `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, \
         `charge_mode`, `charge_quantity`, `status`, `expires_at`, `offer_id`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, 0, 1, 1, CAST(? AS JSON), '0.000', \
         600, 0, 0, 100, 4, 600, 'paid', ?, 101)",
    )
    .bind(format!("ri-{}", rand_suffix()))
    .bind(format!("rc-{}", rand_suffix()))
    .bind(charge_order_id as i64)
    .bind(format!("M{charge_order_id}"))
    .bind(payment_id as i64)
    .bind(USER_ID as i64)
    .bind(format!("openid-{charge_order_id}"))
    .bind(&device_id)
    .bind(format!("pc-{charge_order_id}"))
    .bind(serde_json::json!({"mode": "server_energy"}).to_string())
    .bind(chrono::Utc::now() + chrono::Duration::minutes(5))
    .execute(&db)
    .await
    .unwrap();
    let stop_id = format!("stop-{}", rand_suffix());
    sqlx::query(&format!(
        "INSERT INTO `charge_command` (`command_id`, `stop_command_id`, `charge_order_id`, \
         `payment_order_id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, \
         `port_id`, `owns_port`, `status`, `created_at`) \
         VALUES ('{command_id}', '{stop_id}', {charge_order_id}, {payment_id}, 'C{charge_order_id}', \
         {USER_ID}, '{device_id}', 1, 'pc-{charge_order_id}', {charge_order_id}, 1, 'sent', \
         DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE))"
    ))
    .execute(&db)
    .await
    .unwrap();

    // 兜底扫描（超时阈值 5 分钟 < 10 分钟）。
    let handled = chargepilot_mod_charging::sweeper::sweep_once(&db, 5)
        .await
        .unwrap();
    assert!(handled >= 1, "应至少处理本订单");

    let status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "refunding", "超时出口走退款流程（failed 保持死值）");
    let refunds: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `refund_record` WHERE `biz_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(refunds, 1, "登记自动退款记录");
    let cmd_status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `charge_command` WHERE `command_id` = ?",
    )
    .bind(&command_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(cmd_status, "rejected", "命令标记拒绝");

    // 退款执行链闭环：执行器按 refund_record 领取，成功后收尾 refunding → refunded。
    chargepilot_mod_settlement::refund_executor::run_once(&db, 50)
        .await
        .unwrap();
    let status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        status, "refunded",
        "退款成功后订单收尾 refunded（不再滞留中间态）"
    );

    cleanup_order(&db, charge_order_id, &device_id, "").await;
}

/// 场景 2：事件投递器故障——失败退避后重放成功，命令生成且事件标记 published。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn event_dispatch_failure_replays() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 4).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let device_id = format!("resil-{}", rand_suffix());
    let event_id = format!("evt-resil-{}", rand_suffix());

    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, \
         `status`, `created_month`) VALUES (?, ?, ?, ?, 1, ?, 'paid', ?)",
    )
    .bind(charge_order_id as i64)
    .bind(format!("C{charge_order_id}"))
    .bind(USER_ID as i64)
    .bind(&device_id)
    .bind(format!("pc-{charge_order_id}"))
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&db)
    .await
    .unwrap();
    let payment_id = alloc_next_payment_id(&alloc).await;
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `user_id`, `biz_type`, `biz_id`, \
         `pay_method`, `total_cents`, `paid_cents`, `status`, `created_month`) \
         VALUES (?, ?, ?, 'charge', ?, 'wechat', 100, 100, 'paid', ?)",
    )
    .bind(payment_id as i64)
    .bind(format!("P{payment_id}"))
    .bind(USER_ID as i64)
    .bind(charge_order_id as i64)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `charge_order_id`, \
         `merchant_order_no`, `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, \
         `port_code`, `station_id`, `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, \
         `estimated_kwh`, `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, \
         `charge_mode`, `charge_quantity`, `status`, `expires_at`, `offer_id`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, 0, 1, 1, CAST(? AS JSON), '0.000', \
         600, 0, 0, 100, 4, 600, 'paid', ?, 101)",
    )
    .bind(format!("ri-{}", rand_suffix()))
    .bind(format!("rc-{}", rand_suffix()))
    .bind(charge_order_id as i64)
    .bind(format!("M{charge_order_id}"))
    .bind(payment_id as i64)
    .bind(USER_ID as i64)
    .bind(format!("openid-{charge_order_id}"))
    .bind(&device_id)
    .bind(format!("pc-{charge_order_id}"))
    .bind(serde_json::json!({"mode": "server_energy"}).to_string())
    .bind(chrono::Utc::now() + chrono::Duration::minutes(5))
    .execute(&db)
    .await
    .unwrap();

    // 事务内入队（与业务写入同事务的语义在真实链路由回调保证；此处单独提交）。
    {
        let mut conn = db.acquire().await.unwrap();
        chargepilot_events::enqueue::in_tx(
            &mut conn,
            chargepilot_events::EventSource::User,
            "charge_payment_confirmed",
            &event_id,
            &serde_json::json!({"charge_order_id": charge_order_id}),
        )
        .await
        .unwrap();
    }

    // 场景隔离：claim 按全库 pending/failed 且退避到期领取（LIMIT 100）。
    // 多次全量跑会积累短退避到期的遗留 failed 事件并挤占批次——把非本场景
    // 的到期事件统一推迟 1 小时，使本场景两个批次的领取结果可断言。
    sqlx::query(
        "UPDATE `domain_event` SET `scheduled_at` = DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 1 HOUR) \
         WHERE `status` IN ('pending','failed') AND `scheduled_at` <= UTC_TIMESTAMP(3) \
         AND `event_id` <> ?",
    )
    .bind(&event_id)
    .execute(&db)
    .await
    .unwrap();

    // 首轮：投递器故障（claim 后直接 mark_failed，退避 1 秒）。
    let batch = chargepilot_events::repo::claim_batch(&db, 100)
        .await
        .unwrap();
    let mine = batch
        .iter()
        .find(|e| e.event_id == event_id)
        .expect("事件被领取");
    chargepilot_events::repo::mark_failed(&db, mine.id, 0, "模拟投递器故障")
        .await
        .unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `domain_event` WHERE `event_id` = ?",
    )
    .bind(&event_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(status, "failed", "故障后标记 failed");

    // 退避到期重放：注册处理器（simulation 闭环）后消费成功。
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let dispatcher = chargepilot_mod_charging::StartDispatcher {
        pool: db.clone(),
        allocator: alloc,
        sink: Arc::new(chargepilot_mod_charging::SimulationSink),
        simulation: true,
    };
    let batch = chargepilot_events::repo::claim_batch(&db, 100)
        .await
        .unwrap();
    let mine = batch
        .iter()
        .find(|e| e.event_id == event_id)
        .expect("重放被领取");
    dispatcher.dispatch(mine).await.unwrap();
    chargepilot_events::repo::mark_published(&db, mine.id)
        .await
        .unwrap();

    let commands: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_command` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(commands, 1, "重放生成启动命令（且不重复）");
    let status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `domain_event` WHERE `event_id` = ?",
    )
    .bind(&event_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(status, "published", "事件最终投递完成");

    cleanup_order(&db, charge_order_id, &device_id, &event_id).await;
}

/// 场景 3：进程重启——结束回执同内容重放幂等，状态收敛且计费任务不翻倍。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn restart_replays_end_receipt_idempotently() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 4).await.unwrap();
    let alloc = SegmentAllocator::new(db.clone(), 60);
    alloc.warm_up().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let device_id = format!("resil-{}", rand_suffix());
    let start_command_id = format!("cmd-{}", rand_suffix());

    sqlx::query("INSERT INTO `device_meta` (`device_id`, `protocol_adapter`) VALUES (?, 'dc589')")
        .bind(&device_id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `status`, \
         `started_at`, `created_month`) VALUES (?, ?, ?, ?, 1, 'charging', UTC_TIMESTAMP(3), ?)",
    )
    .bind(charge_order_id as i64)
    .bind(format!("C{charge_order_id}"))
    .bind(USER_ID as i64)
    .bind(&device_id)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&db)
    .await
    .unwrap();
    // 端口占用（启动成功态：结束回执按订单释放）。
    sqlx::query(
        "INSERT INTO `active_port_charge` (`port_id`, `device_id`, `port_no`, `charge_order_id`, \
         `user_id`, `started_at`) VALUES (?, ?, 1, ?, 9003, UTC_TIMESTAMP(3))",
    )
    .bind(charge_order_id as i64)
    .bind(&device_id)
    .bind(charge_order_id as i64)
    .execute(&db)
    .await
    .unwrap();
    // 启动凭据（结束回执校验启动命令一致）。
    sqlx::query(
        "INSERT INTO `charge_start_receipt` (`charge_order_id`, `command_id`, `success`, `port_id`, \
         `order_no`, `device_id`, `port_no`, `result_code`, `occurred_at`) \
         VALUES (?, ?, 1, ?, ?, ?, 1, 0, UTC_TIMESTAMP(3))",
    )
    .bind(charge_order_id as i64)
    .bind(&start_command_id)
    .bind(charge_order_id as i64)
    .bind(format!("C{charge_order_id}"))
    .bind(&device_id)
    .execute(&db)
    .await
    .unwrap();

    let meter = chargepilot_domain::charging_actual::EndMeter {
        segments: vec![],
        charged_wh: 500,
        charged_seconds: 600,
        ended_at: chrono::Utc::now() + chrono::Duration::seconds(2),
        stop_reason: 7,
    };
    let input = chargepilot_mod_charging::receipts::EndReceiptInput {
        charge_order_id,
        order_no: format!("C{charge_order_id}"),
        start_command_id: start_command_id.clone(),
        stop_command_id: format!("stop-{}", rand_suffix()),
        device_id: device_id.clone(),
        port_no: 1,
        meter,
    };

    // 首次应用（进程 A）。
    let replay_first = chargepilot_mod_charging::receipts::apply_end_receipt(&db, &input)
        .await
        .unwrap();
    assert!(!replay_first, "首次应用非重放");
    // 「重启后」重放同一事件（进程 B，同输入）。
    let replay_second = chargepilot_mod_charging::receipts::apply_end_receipt(&db, &input)
        .await
        .unwrap();
    assert!(replay_second, "同内容重放确认");

    let status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(status, "completed", "状态收敛");
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(jobs, 1, "计费任务不翻倍");
    let ports: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `active_port_charge` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(ports, 0, "端口占用已释放");

    cleanup_order(&db, charge_order_id, &device_id, "").await;
}

/// 场景 4：遥测缓冲满——丢弃计数增长，落库明细与聚合完好。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn telemetry_buffer_overflow_drops_oldest() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let db = chargepilot_storage::open_pool(&db_url, 4).await.unwrap();
    let device_id = format!("resil-buf-{}", rand_suffix());
    let buffer = TelemetryBuffer::new();
    let base = chrono::Utc::now();
    let total = 4200_usize;
    for i in 0..total {
        buffer.push(chargepilot_gateway_runtime::telemetry::Sample {
            device_id: device_id.clone(),
            port_no: Some(1),
            metric: "power_w",
            value: f64::from(i as u16),
            ts: base + chrono::Duration::milliseconds(i as i64),
        });
    }
    let dropped = buffer.dropped();
    assert!(
        dropped >= (total - chargepilot_gateway_runtime::telemetry::BUFFER_CAPACITY) as u64,
        "满后丢弃计数增长：dropped={dropped}"
    );
    let samples = buffer.drain();
    assert_eq!(
        samples.len(),
        chargepilot_gateway_runtime::telemetry::BUFFER_CAPACITY
    );
    chargepilot_gateway_runtime::telemetry::flush(&db, &samples)
        .await
        .unwrap();

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM `telemetry` WHERE `device_id` = ?")
        .bind(&device_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        rows,
        chargepilot_gateway_runtime::telemetry::BUFFER_CAPACITY as i64,
        "保留的是最新 4096 条明细"
    );
    // 聚合行存在（15 分钟桶）。
    let aggs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `telemetry_aggregate_15min` WHERE `device_id` = ?",
    )
    .bind(&device_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(aggs >= 1, "聚合完好");

    let _ = sqlx::query("DELETE FROM `telemetry` WHERE `device_id` = ?")
        .bind(&device_id)
        .execute(&db)
        .await;
    let _ = sqlx::query("DELETE FROM `telemetry_aggregate_15min` WHERE `device_id` = ?")
        .bind(&device_id)
        .execute(&db)
        .await;
}
