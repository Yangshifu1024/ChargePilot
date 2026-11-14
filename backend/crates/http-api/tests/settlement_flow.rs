//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 04 批 5 主缝集成测试：计费批次（费用回执幂等/金额封顶/余款退款登记）
//! 与退款执行器（支付单推进/订单 refunded/充值退款回账）。

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

/// 计费批次种子的关联记录，供尾部自清理。
struct SettledSeed {
    charge_order_id: u64,
    payment_id: u64,
    user_id: u64,
    device_id: String,
}

/// 测试方案：amount 套餐 100 分，电价 50+30 分/kWh（种子与快照共用同一份定义）。
fn test_scheme() -> Value {
    json!({
        "name": "结算测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    })
}

/// 种子：一份电量计费方案规则。
async fn seed_rule(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> u64 {
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '结算测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `version`, `status`, `spec_json`) VALUES (?, '结算测试规则', ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    rule_id
}

/// 种子：一笔完成充电的订单（paid 支付单 + completed 订单 + 计费快照 + 结束计量 + 计费任务）。
/// 快照形态与真实 checkout 冻结一致：rule.spec 为套餐引擎规格，offer 携带 station_id。
async fn seed_completed_charge(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    station_id: u64,
    rule_id: u64,
    paid_cents: i64,
    charged_wh: u32,
) -> SettledSeed {
    let payment_id = alloc.next_id().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-s5-{}", rand_suffix());
    let merchant_order_no = format!("P{payment_id}");
    let order_no = format!(
        "C{}{}01",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        device_id
    );
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '结算测试用户', 'active')")
        .bind(user_id as i64).bind(format!("s5-{user_id}")).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'charge', ?, ?, 'wechat', ?, ?, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_id as i64).bind(&merchant_order_no).bind(charge_order_id as i64)
        .bind(user_id as i64).bind(paid_cents).bind(paid_cents).bind(month)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `started_at`, `ended_at`, `charged_kwh`, `charged_seconds`, `created_month`) VALUES (?, ?, ?, ?, 1, 'pc', ?, 'completed', 'paid', DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE), UTC_TIMESTAMP(3), 0.2000, 600, ?)")
        .bind(charge_order_id as i64).bind(&order_no).bind(user_id as i64).bind(&device_id)
        .bind(payment_id as i64).bind(month)
        .execute(db).await.unwrap();
    // 快照经 domain 真实 API 生成（与 checkout 冻结链一致）：spec_for 产出引擎规格，
    // intent_estimate 产出预估，offer 满足 Valid（amount 套餐 + station_id）。
    let snap: chargepilot_domain::charging_scheme::SpecSnapshot =
        serde_json::from_value(json!({"scheme": test_scheme()})).expect("方案应可解析");
    let scheme = snap.scheme.expect("方案存在");
    let package = scheme.package(1).expect("套餐存在");
    let engine_spec = scheme.spec_for(&package);
    let estimate =
        chargepilot_domain::payment_intent::intent_estimate(&scheme, &package, paid_cents)
            .map_err(|e| anyhow::anyhow!(format!("{e:?}")))
            .expect("预估应成功");
    let offer = chargepilot_domain::charging_settle::Offer {
        id: rule_id * 100 + 1,
        station_id,
        name: "1元".to_string(),
        mode: "amount".to_string(),
        price_cents: paid_cents,
        package_id: 1,
        ..Default::default()
    };
    assert!(offer.valid(), "种子套餐应满足校验");
    let snapshot = chargepilot_mod_payment::checkout::PricingSnapshot {
        rule: chargepilot_domain::charging_actual::Rule {
            id: rule_id,
            station_id,
            version: 1,
            spec: engine_spec,
            ..Default::default()
        },
        estimate,
        offer,
        computed_at_unix_micros: now.timestamp_micros(),
    };
    let snapshot_json = serde_json::to_string(&snapshot).unwrap();
    sqlx::query("INSERT INTO `charge_order_pricing` (`charge_order_id`, `payment_intent_id`, `user_id`, `port_code`, `pricing_snapshot`) VALUES (?, ?, ?, 'pc', CAST(? AS JSON))")
        .bind(charge_order_id as i64).bind(format!("int-{charge_order_id}"))
        .bind(user_id as i64).bind(&snapshot_json)
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
    SettledSeed {
        charge_order_id,
        payment_id,
        user_id,
        device_id,
    }
}

/// 尾部自清理：按主键删除本批种子（流水与事件保留供追溯）。
async fn cleanup(db: &sqlx::MySqlPool, seeds: &[SettledSeed]) {
    for s in seeds {
        let _ = sqlx::query("DELETE FROM `charge_fee_receipt` WHERE `charge_order_id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_end_receipt` WHERE `charge_order_id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_order_pricing` WHERE `charge_order_id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `refund_record` WHERE `payment_order_id` = ?")
            .bind(s.payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
            .bind(s.payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(s.user_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
            .bind(&s.device_id)
            .execute(db)
            .await;
    }
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn billing_batch_and_refund_executor() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db) = open_db().await else {
        return;
    };
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 60));
    alloc.warm_up().await.unwrap();
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = seed_rule(db.as_ref(), &alloc).await;

    // 场景 A：预付 100 分，实耗 200Wh（实费 16 分）→ 余款 84 分自动退款。
    let seed_a = seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, 100, 200).await;
    let handled =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed_a.charge_order_id)
            .await
            .expect("计费任务应成功");
    assert!(handled, "任务应完成");

    // 库断言：订单费用列 + 回执无缺口 + 余款退款记录 + 订单 refunding + 任务 done。
    let (electric, service, total): (i64, i64, i64) = sqlx::query_as(
        "SELECT `electric_cents`, `service_cents`, `total_cents` FROM `charge_order` WHERE `id` = ?",
    )
    .bind(seed_a.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (electric, service, total),
        (10, 6, 16),
        "0.2kWh 电费 10 分 + 服务费 6 分"
    );
    let shortfall: i64 = sqlx::query_scalar(
        "SELECT `shortfall_cents` FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(seed_a.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(shortfall, 0);
    let (refund_cents, refund_status, policy): (i64, String, String) = sqlx::query_as(
        "SELECT `refund_cents`, `status`, `execution_policy` FROM `refund_record` \
         WHERE `payment_order_id` = ? AND `refund_no` LIKE 'FEE-R%'",
    )
    .bind(seed_a.payment_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(refund_cents, 84, "余款 84 分应登记自动退款");
    assert_eq!(refund_status, "pending");
    assert_eq!(policy, "automatic");
    let order_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
            .bind(seed_a.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(order_status, "refunding", "余款退款登记后订单应 refunding");
    let job_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(seed_a.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job_status, "done");
    // done 任务不再被领取（批次入口收敛）。
    let due: Vec<u64> = chargepilot_mod_settlement::billing::due_jobs(db.as_ref(), 50)
        .await
        .unwrap();
    assert!(
        !due.contains(&seed_a.charge_order_id),
        "done 任务不应再被领取"
    );

    // 人工把 done 任务置回 pending 重跑：订单已 refunding，应转人工且不重复退款。
    sqlx::query("UPDATE `charge_billing_job` SET `status` = 'pending' WHERE `charge_order_id` = ?")
        .bind(seed_a.charge_order_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let rerun =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed_a.charge_order_id)
            .await
            .expect("重跑不应报错");
    assert!(!rerun, "非 completed 订单应转人工而非重复入账");
    let job_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(seed_a.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job_status, "manual_review");
    let refund_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `refund_record` WHERE `payment_order_id` = ? AND `refund_no` LIKE 'FEE-R%'",
    )
    .bind(seed_a.payment_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(refund_count, 1, "重跑不得重复登记退款");

    // 退款执行器：余款 84 分退清 → 支付单 refunded + 订单 refunded。
    let executed = chargepilot_mod_settlement::refund_executor::run_once(db.as_ref(), 50)
        .await
        .expect("退款执行应成功");
    assert!(executed >= 1);
    let (pay_status, refunded): (String, i64) =
        sqlx::query_as("SELECT `status`, `refunded_cents` FROM `payment_order` WHERE `id` = ?")
            .bind(seed_a.payment_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(refunded, 84);
    assert_eq!(
        pay_status, "partial_refunded",
        "退余款 84 分后支付单应 partial_refunded（16 分为实际消费，不退）"
    );
    let order_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
            .bind(seed_a.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(order_status, "refunded");
    let refund_final: String = sqlx::query_scalar(
        "SELECT `status` FROM `refund_record` WHERE `payment_order_id` = ? AND `refund_no` LIKE 'FEE-R%'",
    )
    .bind(seed_a.payment_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(refund_final, "success");
    // 执行器重跑：无重复退款。
    let _ = chargepilot_mod_settlement::refund_executor::run_once(db.as_ref(), 50).await;
    let refunded_again: i64 =
        sqlx::query_scalar("SELECT `refunded_cents` FROM `payment_order` WHERE `id` = ?")
            .bind(seed_a.payment_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(refunded_again, 84, "重跑不得重复退款");

    // 场景 B：金额封顶——预付 100 分、实耗 5000Wh（理论 400 分）→ 封顶 100 分，缺口 300 分。
    let seed_b = seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, 100, 5000).await;
    let handled_b =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed_b.charge_order_id)
            .await
            .expect("封顶计费任务应成功");
    assert!(handled_b);
    let (total_b, shortfall_b, electric_b, service_b): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT c.`total_cents`, r.`shortfall_cents`, c.`electric_cents`, c.`service_cents` \
         FROM `charge_order` c \
         JOIN `charge_fee_receipt` r ON r.`charge_order_id` = c.`id` WHERE c.`id` = ?",
    )
    .bind(seed_b.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(total_b, 100, "金额模式实费 400 分超出预付应封顶在预付");
    assert_eq!(
        shortfall_b, 0,
        "回执缺口恒 0（旧版语义）：入账总额封顶后不超预付，账单缺口同样恒 0"
    );
    assert_eq!(
        (electric_b, service_b),
        (62, 38),
        "封顶后分项按比例分摊：250×100/400=62 电费 + 38 服务费"
    );

    // 场景 C：实耗 0Wh（实费 0）→ 余款 100 分全额退回，订单 refunding。
    let seed_c = seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, 100, 0).await;
    let handled_c =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed_c.charge_order_id)
            .await
            .expect("零电量计费应成功");
    assert!(handled_c);
    let (total_c, order_status_c): (i64, String) =
        sqlx::query_as("SELECT `total_cents`, `status` FROM `charge_order` WHERE `id` = ?")
            .bind(seed_c.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(total_c, 0);
    assert_eq!(order_status_c, "refunding");
    let surplus_c: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(`refund_cents`), 0) AS SIGNED) FROM `refund_record` WHERE `payment_order_id` = ?",
    )
    .bind(seed_c.payment_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(surplus_c, 100, "实费为 0 时预付金额应全额登记退款");

    // 场景 D：计量时长矛盾（charged_seconds 超出区间 5 分钟容差）→ 转人工，不入账。
    let seed_d = seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, 100, 200).await;
    let bad_meter = json!({
        "segments": [{"started_at": (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
                      "ended_at": chrono::Utc::now().to_rfc3339(), "energy_wh": 200, "peak_w": 1500}],
        "charged_wh": 200, "charged_seconds": 3600,
        "ended_at": chrono::Utc::now().to_rfc3339(), "stop_reason": 0
    });
    sqlx::query("UPDATE `charge_end_receipt` SET `meter_json` = CAST(? AS JSON) WHERE `charge_order_id` = ?")
        .bind(bad_meter.to_string())
        .bind(seed_d.charge_order_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let handled_d =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed_d.charge_order_id)
            .await
            .expect("异常计量应安全处理");
    assert!(!handled_d, "异常计量应转人工");
    let job_d: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(seed_d.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job_d, "manual_review");
    let fee_d: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_fee_receipt` WHERE `charge_order_id` = ?")
            .bind(seed_d.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(fee_d, 0, "转人工不得入账");

    cleanup(db.as_ref(), &[seed_a, seed_b, seed_c, seed_d]).await;
    let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
        .bind(station_id as i64)
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(rule_id as i64)
        .execute(db.as_ref())
        .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn wallet_recharge_refund_returns_balance() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db) = open_db().await else {
        return;
    };
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 60));
    alloc.warm_up().await.unwrap();
    // 种子：用户 + 钱包 500 分 + 已支付充值支付单 500 分 + pending 自动退款 500 分。
    let user_id = alloc.next_id().await.unwrap();
    let payment_id = alloc.next_id().await.unwrap();
    let refund_id = alloc.next_id().await.unwrap();
    let request_id = chargepilot_platform::ids::uuid_v4();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '结算测试用户', 'active')")
        .bind(user_id as i64).bind(format!("s5-{user_id}")).execute(db.as_ref()).await.unwrap();
    sqlx::query("INSERT INTO `wallet_account` (`id`, `user_id`, `balance_cents`, `frozen_cents`, `version`, `status`) VALUES (?, ?, 500, 0, 1, 'active')")
        .bind(alloc.next_id().await.unwrap() as i64).bind(user_id as i64)
        .execute(db.as_ref()).await.unwrap();
    sqlx::query("INSERT INTO `wallet_recharge_request` (`request_id`, `user_id`, `amount_cents`, `payment_order_id`, `request_json`) VALUES (?, ?, 500, ?, CAST('{}' AS JSON))")
        .bind(&request_id).bind(user_id as i64).bind(payment_id as i64)
        .execute(db.as_ref()).await.unwrap();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'wallet_recharge', ?, ?, 'wechat', 500, 500, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_id as i64).bind(format!("P{payment_id}")).bind(user_id as i64)
        .bind(user_id as i64)
        .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
        .execute(db.as_ref()).await.unwrap();
    sqlx::query("INSERT INTO `refund_record` (`id`, `refund_no`, `payment_order_id`, `user_id`, `biz_type`, `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `next_attempt_at`, `created_month`) VALUES (?, ?, ?, ?, 'wallet_recharge', ?, 500, '测试退款', 'pending', 'automatic', UTC_TIMESTAMP(3), ?)")
        .bind(refund_id as i64).bind(format!("WR{refund_id:016x}")).bind(payment_id as i64)
        .bind(user_id as i64).bind(user_id as i64)
        .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
        .execute(db.as_ref()).await.unwrap();

    let executed = chargepilot_mod_settlement::refund_executor::run_once(db.as_ref(), 50)
        .await
        .expect("充值退款应成功");
    assert!(executed >= 1);
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance, 1000, "充值退款应回账 500 分");
    let pay_status: String =
        sqlx::query_scalar("SELECT `status` FROM `payment_order` WHERE `id` = ?")
            .bind(payment_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(pay_status, "refunded");
    let ledger_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `wallet_txn` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert!(ledger_rows >= 1, "退款回账应落钱包流水");

    // 尾部自清理（流水保留供追溯）。
    let _ = sqlx::query("DELETE FROM `wallet_recharge_request` WHERE `request_id` = ?")
        .bind(&request_id)
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `refund_record` WHERE `id` = ?")
        .bind(refund_id as i64)
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
        .bind(payment_id as i64)
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `wallet_account` WHERE `user_id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await;
}

/// 防止 Value 导入未使用告警（json! 宏展开隐式使用）。
#[allow(dead_code)]
fn _assert_value_type(_: &Value) {}
