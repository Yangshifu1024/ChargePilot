//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 04 批 6b 主缝集成测试：账单签发/已读、人工退款双人审核（批准后由
//! 退款执行器接手）、仪表审核修正重排队与人工最终金额入账。

use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use std::sync::Arc;

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// 资金批次互斥锁（共享 dev 库的 flow 测试串行）。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn open_db() -> Option<Arc<sqlx::MySqlPool>> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => Some(Arc::new(d)),
        Err(e) => {
            panic!("数据库连接失败：{e}");
        }
    }
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 测试方案（种子与快照共用同一份定义）。
fn test_scheme() -> Value {
    json!({
        "name": "账单测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    })
}

/// 复用 settlement_flow 的种子形态：完成充电订单（paid 100 分 + 200Wh 计量 + 任务）。
/// 返回 (charge_order_id, payment_id, user_id, rule_id)。
async fn seed_completed_charge(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    charged_wh: u32,
) -> (u64, u64, u64, u64) {
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    let payment_id = alloc.next_id().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-s7-{}", rand_suffix());
    let order_no = format!(
        "C{}{}01",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        device_id
    );
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '账单测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `version`, `status`, `spec_json`) VALUES (?, '账单测试规则', ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '账单测试用户', 'active')")
        .bind(user_id as i64).bind(format!("s7-{user_id}")).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'charge', ?, ?, 'wechat', 100, 100, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_id as i64).bind(format!("P{payment_id}")).bind(charge_order_id as i64)
        .bind(user_id as i64).bind(month)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `started_at`, `ended_at`, `charged_kwh`, `charged_seconds`, `created_month`) VALUES (?, ?, ?, ?, 1, 'pc', ?, 'completed', 'paid', DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE), UTC_TIMESTAMP(3), 0.2000, 600, ?)")
        .bind(charge_order_id as i64).bind(&order_no).bind(user_id as i64).bind(&device_id)
        .bind(payment_id as i64).bind(month)
        .execute(db).await.unwrap();
    // 快照经 domain 真实 API 生成（与 checkout 冻结链一致，见 settlement_flow）。
    let snap: chargepilot_domain::charging_scheme::SpecSnapshot =
        serde_json::from_value(json!({"scheme": test_scheme()})).expect("方案应可解析");
    let scheme = snap.scheme.expect("方案存在");
    let package = scheme.package(1).expect("套餐存在");
    let estimate = chargepilot_domain::payment_intent::intent_estimate(&scheme, &package, 100)
        .map_err(|e| anyhow::anyhow!(format!("{e:?}")))
        .expect("预估应成功");
    let snapshot = chargepilot_mod_payment::checkout::PricingSnapshot {
        rule: chargepilot_domain::charging_actual::Rule {
            id: rule_id,
            station_id,
            version: 1,
            spec: scheme.spec_for(&package),
            ..Default::default()
        },
        estimate,
        offer: chargepilot_domain::charging_settle::Offer {
            id: rule_id * 100 + 1,
            station_id,
            name: "1元".to_string(),
            mode: "amount".to_string(),
            price_cents: 100,
            package_id: 1,
            ..Default::default()
        },
        computed_at_unix_micros: now.timestamp_micros(),
    };
    let snapshot = serde_json::to_value(&snapshot).expect("快照序列化");
    sqlx::query("INSERT INTO `charge_order_pricing` (`charge_order_id`, `payment_intent_id`, `user_id`, `port_code`, `pricing_snapshot`) VALUES (?, ?, ?, 'pc', CAST(? AS JSON))")
        .bind(charge_order_id as i64).bind(format!("int-{charge_order_id}"))
        .bind(user_id as i64).bind(snapshot.to_string())
        .execute(db).await.unwrap();
    let meter = json!({
        "segments": [{"started_at": (now - chrono::Duration::minutes(10)).to_rfc3339(),
                      "ended_at": now.to_rfc3339(), "energy_wh": charged_wh, "peak_w": 1500}],
        "charged_wh": charged_wh, "charged_seconds": 600,
        "ended_at": now.to_rfc3339(), "stop_reason": 0
    });
    sqlx::query("INSERT INTO `charge_end_receipt` (`charge_order_id`, `stop_command_id`, `meter_json`) VALUES (?, ?, CAST(? AS JSON))")
        .bind(charge_order_id as i64).bind(format!("stop-{charge_order_id}"))
        .bind(meter.to_string()).execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO `charge_billing_job` (`charge_order_id`, `status`) VALUES (?, 'pending')",
    )
    .bind(charge_order_id as i64)
    .execute(db)
    .await
    .unwrap();
    (charge_order_id, payment_id, user_id, rule_id)
}

/// 尾部自清理（按订单/支付主键）。
async fn cleanup(db: &sqlx::MySqlPool, ids: &[(u64, u64, u64, u64)]) {
    for (order_id, payment_id, user_id, rule_id) in ids {
        for table in [
            "charge_bill",
            "charge_manual_settlement",
            "charge_meter_review",
            "charge_fee_receipt",
            "charge_billing_job",
            "charge_end_receipt",
            "charge_order_pricing",
            "charge_order",
        ] {
            let _ = sqlx::query(&format!(
                "DELETE FROM `{table}` WHERE `charge_order_id` = ?"
            ))
            .bind(*order_id as i64)
            .execute(db)
            .await;
        }
        let _ = sqlx::query(
            "DELETE br FROM `charge_bill_read` br \
             JOIN `charge_bill` b ON b.`id` = br.`bill_id` WHERE b.`charge_order_id` = ?",
        )
        .bind(*order_id as i64)
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE x FROM `refund_rejection` x \
             JOIN `refund_record` r ON r.`id` = x.`refund_record_id` WHERE r.`payment_order_id` = ?")
            .bind(*payment_id as i64).execute(db).await;
        let _ = sqlx::query("DELETE x FROM `refund_review` x \
             JOIN `refund_record` r ON r.`id` = x.`refund_record_id` WHERE r.`payment_order_id` = ?")
            .bind(*payment_id as i64).execute(db).await;
        let _ = sqlx::query(
            "DELETE x FROM `manual_refund_request` x \
             JOIN `refund_record` r ON CAST(r.`refund_no` AS CHAR) = x.`refund_no` \
             WHERE r.`payment_order_id` = ?",
        )
        .bind(*payment_id as i64)
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `refund_record` WHERE `payment_order_id` = ?")
            .bind(*payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
            .bind(*payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(*user_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
            .bind(*rule_id as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query("DELETE FROM `station` WHERE `name` = '账单测试站'")
        .execute(db)
        .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn bill_issue_and_manual_refund_review() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db) = open_db().await else {
        return;
    };
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 60));
    alloc.warm_up().await.unwrap();
    let (order_id, payment_id, user_id, rule_id) =
        seed_completed_charge(db.as_ref(), &alloc, 2000).await;

    // 1. 计费入账（实费 160 分 → 封顶预付 100 分，无余款退款）。
    let handled = chargepilot_mod_settlement::billing::process_job(db.as_ref(), order_id)
        .await
        .expect("计费应成功");
    assert!(handled);

    // 2. 账单签发：16 分实费 + prepaid 100 + shortfall 0；重复签发幂等。
    let bill = chargepilot_mod_settlement::bill::issue_bill(db.as_ref(), order_id)
        .await
        .expect("签发应成功")
        .expect("已计费订单应产出账单");
    assert_eq!(bill.total_cents, 100, "实费 160 分封顶在预付 100 分");
    assert_eq!(bill.prepaid_cents, 100);
    assert_eq!(
        bill.shortfall_cents, 0,
        "入账总额未超预付（封顶），账单无缺口（旧版推导仅对历史异常兜底）"
    );
    let again = chargepilot_mod_settlement::bill::issue_bill(db.as_ref(), order_id)
        .await
        .expect("重复签发应成功")
        .expect("应复用既有账单");
    assert_eq!(again.id, bill.id, "重复签发应复用同一账单");
    let bill_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_bill` WHERE `charge_order_id` = ?")
            .bind(order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(bill_count, 1);
    let event: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_event_log` WHERE `charge_order_id` = ? AND `event` = 'bill_issued'",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(event, 1, "账单事件只追加一次");
    // 用户已读。
    let read = chargepilot_mod_settlement::bill::mark_bill_read(db.as_ref(), user_id, bill.id)
        .await
        .expect("已读应成功");
    assert!(read);
    let other: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_bill_read` WHERE `bill_id` = ?")
            .bind(bill.id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(other, 1);

    // 3. 人工退款登记（订单保持 completed：实费封顶无余款退款在途）。
    let request_id = chargepilot_platform::ids::uuid_v4();
    let (created, refund_no) = chargepilot_mod_settlement::refund_admin::request_manual_refund(
        db.as_ref(),
        None,
        9001,
        order_id,
        &request_id,
        50,
        "测试人工退款",
    )
    .await
    .expect("人工退款登记应成功");
    assert!(created);
    assert!(refund_no.starts_with("MR"));
    // 同请求重放。
    let (replayed, no2) = chargepilot_mod_settlement::refund_admin::request_manual_refund(
        db.as_ref(),
        None,
        9001,
        order_id,
        &request_id,
        50,
        "测试人工退款",
    )
    .await
    .expect("重放应成功");
    assert!(!replayed);
    assert_eq!(no2, refund_no);
    // 内容不同冲突。
    let err = chargepilot_mod_settlement::refund_admin::request_manual_refund(
        db.as_ref(),
        None,
        9001,
        order_id,
        &request_id,
        60,
        "测试人工退款",
    )
    .await
    .expect_err("内容不同应冲突");
    assert_eq!(
        err,
        chargepilot_mod_settlement::refund_admin::RefundAdminError::Conflict
    );

    // 4. 双人审核：第一审 → awaiting_second；第二审（不同人）→ Approved 转 automatic。
    let r1 = chargepilot_mod_settlement::refund_admin::review_manual_refund(
        db.as_ref(),
        None,
        9001,
        &refund_no,
        true,
        "同意退款",
        "",
        Some(true),
    )
    .await
    .expect("首审应成功");
    assert_eq!(
        r1,
        chargepilot_mod_settlement::refund_admin::ReviewOutcome::AwaitingSecond
    );
    // 同人二审冲突。
    let err = chargepilot_mod_settlement::refund_admin::review_manual_refund(
        db.as_ref(),
        None,
        9001,
        &refund_no,
        true,
        "再同意",
        "",
        Some(true),
    )
    .await
    .expect_err("同人二审应冲突");
    assert_eq!(
        err,
        chargepilot_mod_settlement::refund_admin::RefundAdminError::Conflict
    );
    // 首审人权限失效冲突。
    let err = chargepilot_mod_settlement::refund_admin::review_manual_refund(
        db.as_ref(),
        None,
        9002,
        &refund_no,
        true,
        "同意",
        "",
        Some(false),
    )
    .await
    .expect_err("首审人权限失效应冲突");
    assert_eq!(
        err,
        chargepilot_mod_settlement::refund_admin::RefundAdminError::Conflict
    );
    let r2 = chargepilot_mod_settlement::refund_admin::review_manual_refund(
        db.as_ref(),
        None,
        9002,
        &refund_no,
        true,
        "复核同意",
        "",
        Some(true),
    )
    .await
    .expect("二审应成功");
    assert_eq!(
        r2,
        chargepilot_mod_settlement::refund_admin::ReviewOutcome::Approved
    );
    let (policy, status): (String, String) = sqlx::query_as(
        "SELECT `execution_policy`, `status` FROM `refund_record` WHERE CAST(`refund_no` AS CHAR) = ?",
    )
    .bind(&refund_no)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (policy, status),
        ("automatic".to_string(), "pending".to_string()),
        "双审通过应转 automatic 排队"
    );

    // 5. 退款执行器接手：50 分退清 → 支付单 partial_refunded（100 分消费封顶）。
    let executed = chargepilot_mod_settlement::refund_executor::run_once(db.as_ref(), 50)
        .await
        .expect("执行应成功");
    assert!(executed >= 1);
    let refunded: i64 =
        sqlx::query_scalar("SELECT `refunded_cents` FROM `payment_order` WHERE `id` = ?")
            .bind(payment_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(refunded, 50);

    cleanup(db.as_ref(), &[(order_id, payment_id, user_id, rule_id)]).await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn meter_review_and_manual_amount() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db) = open_db().await else {
        return;
    };
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 60));
    alloc.warm_up().await.unwrap();
    let (order_id, payment_id, user_id, rule_id) =
        seed_completed_charge(db.as_ref(), &alloc, 200).await;

    // 场景 A：分段守恒争议转人工 → 提案修正 → 双审批准 → 重排队重算入账。
    // 分段合计 250Wh 与总量 200Wh 不一致（时长自洽）：转人工后走分段修正。
    let now = chrono::Utc::now();
    let bad_meter = json!({
        "segments": [
            {"started_at": (now - chrono::Duration::minutes(10)).to_rfc3339(),
             "ended_at": (now - chrono::Duration::minutes(5)).to_rfc3339(), "energy_wh": 100, "peak_w": 1500},
            {"started_at": (now - chrono::Duration::minutes(5)).to_rfc3339(),
             "ended_at": now.to_rfc3339(), "energy_wh": 150, "peak_w": 1500}
        ],
        "charged_wh": 200, "charged_seconds": 600,
        "ended_at": now.to_rfc3339(), "stop_reason": 0
    });
    sqlx::query("UPDATE `charge_end_receipt` SET `meter_json` = CAST(? AS JSON) WHERE `charge_order_id` = ?")
        .bind(bad_meter.to_string())
        .bind(order_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let handled = chargepilot_mod_settlement::billing::process_job(db.as_ref(), order_id)
        .await
        .expect("异常计量应安全处理");
    assert!(!handled, "应转人工");
    let job: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job, "manual_review");

    // 提案：修正分段守恒（合计 200Wh，与总量一致）。
    let review_id = chargepilot_mod_settlement::meter_review::propose_meter_review(
        db.as_ref(),
        None,
        8001,
        order_id,
        &chargepilot_platform::ids::uuid_v4(),
        "分段合计与总量不一致，修正分段读数",
        &[
            chargepilot_mod_settlement::meter_review::MeterSegmentInput {
                started_at: now - chrono::Duration::minutes(10),
                ended_at: now - chrono::Duration::minutes(5),
                energy_wh: 100,
                peak_w: 1500,
            },
            chargepilot_mod_settlement::meter_review::MeterSegmentInput {
                started_at: now - chrono::Duration::minutes(5),
                ended_at: now,
                energy_wh: 100,
                peak_w: 1500,
            },
        ],
    )
    .await
    .expect("提案应成功");
    assert!(review_id > 0);
    // 首审人二审冲突。
    let err = chargepilot_mod_settlement::meter_review::review_meter(
        db.as_ref(),
        None,
        8001,
        order_id,
        review_id,
        true,
        "",
        Some(true),
    )
    .await
    .expect_err("首审人二审应冲突");
    assert_eq!(
        err,
        chargepilot_mod_settlement::meter_review::ReviewError::Conflict
    );
    // 二审批准 → 任务重排队。
    chargepilot_mod_settlement::meter_review::review_meter(
        db.as_ref(),
        None,
        8002,
        order_id,
        review_id,
        true,
        "",
        Some(true),
    )
    .await
    .expect("二审批准应成功");
    let job: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job, "pending", "批准后应重排队");
    // 重算入账（修正分段 10 分钟 200Wh → 16 分）。
    let handled = chargepilot_mod_settlement::billing::process_job(db.as_ref(), order_id)
        .await
        .expect("重算应成功");
    assert!(handled);
    let total: i64 = sqlx::query_scalar("SELECT `total_cents` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert_eq!(total, 16, "修正计量后应按 200Wh 正常入账");

    // 场景 B：人工最终金额（新订单转人工后登记 40+10 分 → 入账 50 分）。
    let (order_b, payment_b, user_b, rule_b) =
        seed_completed_charge(db.as_ref(), &alloc, 200).await;
    let bad_b = json!({
        "segments": [{"started_at": (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
                      "ended_at": chrono::Utc::now().to_rfc3339(), "energy_wh": 9999, "peak_w": 1500}],
        "charged_wh": 9999, "charged_seconds": 60,
        "ended_at": chrono::Utc::now().to_rfc3339(), "stop_reason": 0
    });
    sqlx::query("UPDATE `charge_end_receipt` SET `meter_json` = CAST(? AS JSON) WHERE `charge_order_id` = ?")
        .bind(bad_b.to_string())
        .bind(order_b as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let _ = chargepilot_mod_settlement::billing::process_job(db.as_ref(), order_b).await;
    let job_b: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(order_b as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job_b, "manual_review", "异常计量应先转人工");
    // 人工金额合计 50 ≤ 套餐价 100。
    chargepilot_mod_settlement::meter_review::resolve_manual_amount(
        db.as_ref(),
        None,
        8003,
        order_b,
        &chargepilot_platform::ids::uuid_v4(),
        40,
        10,
        "协商金额",
    )
    .await
    .expect("人工金额应成功");
    let handled_b = chargepilot_mod_settlement::billing::process_job(db.as_ref(), order_b)
        .await
        .expect("人工金额入账应成功");
    assert!(handled_b);
    let (electric_b, service_b, total_b): (i64, i64, i64) = sqlx::query_as(
        "SELECT `electric_cents`, `service_cents`, `total_cents` FROM `charge_order` WHERE `id` = ?",
    )
    .bind(order_b as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (electric_b, service_b, total_b),
        (40, 10, 50),
        "人工金额应直接入账"
    );
    // 超套餐价拒绝。
    let err = chargepilot_mod_settlement::meter_review::resolve_manual_amount(
        db.as_ref(),
        None,
        8003,
        order_b,
        &chargepilot_platform::ids::uuid_v4(),
        999,
        999,
        "超限",
    )
    .await
    .expect_err("已有人工金额且超限应冲突");
    assert_eq!(
        err,
        chargepilot_mod_settlement::meter_review::ReviewError::Conflict
    );

    cleanup(
        db.as_ref(),
        &[
            (order_id, payment_id, user_id, rule_id),
            (order_b, payment_b, user_b, rule_b),
        ],
    )
    .await;
}
