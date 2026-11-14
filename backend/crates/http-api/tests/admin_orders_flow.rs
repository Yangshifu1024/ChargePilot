#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 后台订单查询主缝集成测试：列表筛选与分页、详情（冻结套餐/计费单号/退款申请人）、
//! 事件时间线、过程采样游标、订单关联退款与支付单列表（对齐旧版契约的响应形状）。
//! 依赖真实 MySQL（TEST_MYSQL_URL）与 Redis（TEST_REDIS_URL），种子尾部自清理。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use support::{TestApp, flow_lock, rand_suffix, req};

/// 超管登录（role_id=1 拥有 order.read 与 order.refund.create），返回令牌与管理员 ID。
async fn login_super(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> (String, u64) {
    let suffix = rand_suffix();
    let username = format!("ord-{suffix}");
    let id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("order-pass-123").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(id as i64)
        .bind(&username)
        .bind(hash)
        .execute(db)
        .await
        .unwrap();
    let (status, body) = req(
        router,
        "POST",
        "/api/v1/admin/auth/login",
        Some(json!({"username": username, "password": "order-pass-123"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "超管登录应成功: {body}");
    let token = body["data"]["access_token"]
        .as_str()
        .expect("登录应返回令牌")
        .to_string();
    (token, id)
}

/// 测试方案：amount 套餐 1000 分（与详情快照断言共用同一份定义）。
fn test_scheme() -> Value {
    json!({
        "name": "订单查询测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 1000}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    })
}

/// 本批次种子：站点、用户、支付单、订单、意图、快照、事件、过程与退款。
struct Seed {
    station_id: u64,
    user_id: u64,
    rule_id: u64,
    /// 完成单：快照 + 计费单号 + 事件 + 过程 + 待审退款。
    order_1: OrderSeed,
    /// 充电中单：card_charge 关联（start_source=card）+ live_unavailable。
    order_2: OrderSeed,
    /// 未启动单：无支付、无站点意图（创建时间回退分支与空集合断言用）。
    order_3: OrderSeed,
    /// 钱包充值支付单（payment-orders 的 biz_type / 无充电关联断言用）。
    pay_3: u64,
    /// order_1 的待审退款记录。
    refund_id: u64,
}

struct OrderSeed {
    id: u64,
    order_no: String,
    device_id: String,
    payment_id: u64,
    /// 充电过程行主键（按插入序）。
    process_ids: Vec<u64>,
}

/// 月份分区键（charge_order / payment_order / refund_record 插入必带）。
fn month() -> chrono::NaiveDate {
    chargepilot_mod_order::month_start_of(chrono::Utc::now())
}

/// 从快照结构生成与 checkout 冻结一致的 PricingSnapshot JSON（结算批次同款）。
fn frozen_snapshot_json(rule_id: u64, station_id: u64) -> anyhow::Result<String> {
    let snap: chargepilot_domain::charging_scheme::SpecSnapshot =
        serde_json::from_value(json!({"scheme": test_scheme()})).expect("方案应可解析");
    let scheme = snap.scheme.expect("方案存在");
    let package = scheme.package(1).expect("套餐存在");
    let engine_spec = scheme.spec_for(&package);
    let estimate = chargepilot_domain::payment_intent::intent_estimate(&scheme, &package, 1000)
        .map_err(|e| anyhow::anyhow!(format!("{e:?}")))?;
    let offer = chargepilot_domain::charging_settle::Offer {
        id: rule_id * 100 + 1,
        station_id,
        name: "1元".to_string(),
        mode: "amount".to_string(),
        price_cents: 1000,
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
        computed_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    Ok(serde_json::to_string(&snapshot)?)
}

/// 种子一笔订单及其关联行。`pay_method` 传 "none" 表示不建支付单与支付意图
/// （未启动单用，验证无有效支付时的空值契约）。
#[allow(clippy::too_many_arguments)]
async fn seed_order(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    station_id: u64,
    rule_id: u64,
    user_id: u64,
    pay_method: &str,
    pay_status: &str,
    order_status: &str,
    payment_status: &str,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<OrderSeed> {
    let order_id = alloc.next_id().await?;
    let device_id = format!("dev-ord-{}", rand_suffix());
    let order_no = format!(
        "C{}{device_id}01",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    let m = month();
    let has_payment = pay_method != "none";
    let payment_id = if has_payment {
        alloc.next_id().await?
    } else {
        0
    };
    let merchant_order_no = format!("P{payment_id}");
    let paid_cents: i64 = if pay_status == "paid" {
        if pay_method == "wechat" { 1000 } else { 500 }
    } else {
        0
    };
    if has_payment {
        sqlx::query(
            "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, \
             `total_cents`, `paid_cents`, `status`, `paid_at`, `created_at`, `created_month`) \
             VALUES (?, ?, 'charge', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(payment_id as i64)
        .bind(&merchant_order_no)
        .bind(order_id as i64)
        .bind(user_id as i64)
        .bind(pay_method)
        .bind(1000i64)
        .bind(paid_cents)
        .bind(pay_status)
        .bind(if paid_cents > 0 {
            Some(chrono::Utc::now())
        } else {
            None
        })
        .bind(created_at)
        .bind(m)
        .execute(db)
        .await?;
    }
    sqlx::query(
        "INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, \
         `payment_order_id`, `status`, `payment_status`, `started_at`, `ended_at`, `charged_kwh`, \
         `charged_seconds`, `electric_cents`, `service_cents`, `total_cents`, `created_at`, `created_month`) \
         VALUES (?, ?, ?, ?, 1, 'pc', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order_id as i64)
    .bind(&order_no)
    .bind(user_id as i64)
    .bind(&device_id)
    .bind(if has_payment {
        Some(payment_id as i64)
    } else {
        None
    })
    .bind(order_status)
    .bind(payment_status)
    .bind(started_at)
    .bind(if order_status == "completed" {
        Some(created_at)
    } else {
        None
    })
    .bind(if order_status == "completed" {
        Some("0.2000")
    } else {
        None
    })
    .bind(if order_status == "completed" { Some(600i64) } else { None })
    .bind(if order_status == "completed" { Some(10i64) } else { None })
    .bind(if order_status == "completed" { Some(6i64) } else { None })
    .bind(if order_status == "completed" { Some(16i64) } else { None })
    .bind(created_at)
    .bind(m)
    .execute(db)
    .await?;
    // 支付意图（列表/详情的 station_id 来源）；无支付或未启动单刻意不建。
    if has_payment && started_at.is_some() {
        let snapshot_json = frozen_snapshot_json(rule_id, station_id)?;
        sqlx::query(
            "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `merchant_order_no`, \
             `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, `port_code`, `station_id`, \
             `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, \
             `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, \
             `charge_quantity`, `status`, `expires_at`, `paid_at`, `charge_order_id`) \
             VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, 1, 'pc', ?, ?, 1, CAST(? AS JSON), \
             '2.500', 120, 125, 75, 200, 4, 1000, 'paid', DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 5 MINUTE), UTC_TIMESTAMP(3), ?)",
        )
        .bind(chargepilot_platform::ids::uuid_v4())
        .bind(chargepilot_platform::ids::uuid_v4())
        .bind(&merchant_order_no)
        .bind(payment_id as i64)
        .bind(user_id as i64)
        .bind(format!("ord-{user_id}"))
        .bind(&device_id)
        .bind(station_id as i64)
        .bind(rule_id as i64)
        .bind(&snapshot_json)
        .bind(order_id as i64)
        .execute(db)
        .await?;
    }
    Ok(OrderSeed {
        id: order_id,
        order_no,
        device_id,
        payment_id,
        process_ids: Vec::new(),
    })
}

/// 给订单补充电过程采样（三行，递增 ID 与计量）。
async fn seed_process(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    order: &mut OrderSeed,
) -> anyhow::Result<()> {
    for i in 0..3u64 {
        let id = alloc.next_id().await?;
        sqlx::query(
            "INSERT INTO `charge_process` (`id`, `event_key`, `charge_order_id`, `order_no`, `device_id`, \
             `port_no`, `ts`, `power_deciwatts`, `charged_seconds`, `remaining_seconds`, `charged_mwh`, \
             `remaining_mwh`, `signal_strength`, `port_status`, `voltage_v`, `temperature_c`, `device_status`) \
             VALUES (?, ?, ?, ?, ?, 1, UTC_TIMESTAMP(3), ?, ?, ?, ?, ?, 20, 1, 220, 25, 2)",
        )
        .bind(id as i64)
        .bind(format!("ev-{id:016x}"))
        .bind(order.id as i64)
        .bind(&order.order_no)
        .bind(&order.device_id)
        .bind((1500 + i * 500) as i64)
        .bind((60 + i * 60) as i64)
        .bind((540 - i * 60) as i64)
        .bind((50000 + i * 50000) as i64)
        .bind((950000 - i * 50000) as i64)
        .execute(db)
        .await?;
        order.process_ids.push(id);
    }
    Ok(())
}

/// 全量种子与整体清空的登记。
async fn seed_all(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> anyhow::Result<Seed> {
    // 防御性预清理：先前运行若中途 panic 未走尾部清理，这里按种子特征兜底
    // （flow 测试被全局锁串行化，不会误删并发批次的种子）。
    for sql in [
        "DELETE p FROM `payment_order` p JOIN `charge_order` c ON c.`payment_order_id` = p.`id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE pay FROM `payment_order` pay WHERE pay.`biz_type` = 'wallet_recharge' AND pay.`user_id` IN (SELECT id FROM `user` WHERE openid LIKE 'ord-%')",
        "DELETE FROM `charge_payment_intent` WHERE `device_id` LIKE 'dev-ord-%'",
        "DELETE t FROM `charge_order_pricing` t JOIN `charge_order` c ON c.`id` = t.`charge_order_id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE t FROM `charge_event_log` t JOIN `charge_order` c ON c.`id` = t.`charge_order_id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE t FROM `charge_process` t JOIN `charge_order` c ON c.`id` = t.`charge_order_id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE t FROM `card_charge` t JOIN `charge_order` c ON c.`id` = t.`charge_order_id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE t FROM `charge_fee_receipt` t JOIN `charge_order` c ON c.`id` = t.`charge_order_id` WHERE c.`device_id` LIKE 'dev-ord-%'",
        "DELETE r FROM `refund_record` r WHERE r.`user_id` IN (SELECT id FROM `user` WHERE openid LIKE 'ord-%')",
        "DELETE FROM `charge_order` WHERE `device_id` LIKE 'dev-ord-%'",
        "DELETE FROM `user` WHERE openid LIKE 'ord-%'",
        "DELETE FROM `station` WHERE `name` = '订单查询测试站'",
        "DELETE FROM `pricing_rule` WHERE `name` = '订单查询测试规则'",
    ] {
        sqlx::query(sql).execute(db).await?;
    }
    let station_id = alloc.next_id().await?;
    let rule_id = alloc.next_id().await?;
    let user_id = alloc.next_id().await?;
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '订单查询测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64)
        .execute(db)
        .await?;
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `version`, `status`, `spec_json`) VALUES (?, '订单查询测试规则', ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64)
        .bind(station_id as i64)
        .bind(spec.to_string())
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '订单查询测试用户', 'active')")
        .bind(user_id as i64)
        .bind(format!("ord-{user_id}"))
        .execute(db)
        .await?;

    let now = chrono::Utc::now();
    // 完成单：启动于 30 分钟前（时间过滤闭区间下界）。
    let mut order_1 = seed_order(
        db,
        alloc,
        station_id,
        rule_id,
        user_id,
        "wechat",
        "paid",
        "completed",
        "paid",
        Some(now - chrono::Duration::minutes(30)),
        now - chrono::Duration::minutes(31),
    )
    .await?;
    // 充电中单：5 分钟前启动（live_unavailable 与 card 来源断言）。
    let order_2 = seed_order(
        db,
        alloc,
        station_id,
        rule_id,
        user_id,
        "balance",
        "paid",
        "charging",
        "paid",
        Some(now - chrono::Duration::minutes(5)),
        now - chrono::Duration::minutes(6),
    )
    .await?;
    // 未启动单：无支付、无意图，创建于 3 小时前（时间过滤应排除）。
    let order_3 = seed_order(
        db,
        alloc,
        station_id,
        rule_id,
        user_id,
        "none",
        "initiated",
        "pending_payment",
        "pending",
        None,
        now - chrono::Duration::hours(3),
    )
    .await?;

    // order_1：冻结快照 + 计费单号 + 两条事件 + 三行过程。
    let snapshot_json = frozen_snapshot_json(rule_id, station_id)?;
    sqlx::query("INSERT INTO `charge_order_pricing` (`charge_order_id`, `payment_intent_id`, `user_id`, `port_code`, `pricing_snapshot`) VALUES (?, ?, ?, 'pc', CAST(? AS JSON))")
        .bind(order_1.id as i64)
        .bind(format!("int-{}", order_1.id))
        .bind(user_id as i64)
        .bind(&snapshot_json)
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO `charge_fee_receipt` (`charge_order_id`, `calculation_no`, `result_json`) VALUES (?, ?, CAST('{}' AS JSON))")
        .bind(order_1.id as i64)
        .bind(format!("QB{}", order_1.id))
        .execute(db)
        .await?;
    for (i, offset_min) in [5i64, 1].iter().enumerate() {
        sqlx::query(
            "INSERT INTO `charge_event_log` (`charge_order_id`, `event_id`, `event`, `actor`, `detail`, `occurred_at`) \
             VALUES (?, ?, ?, 'system', ?, DATE_SUB(UTC_TIMESTAMP(3), INTERVAL ? MINUTE))",
        )
        .bind(order_1.id as i64)
        .bind(format!("evt-{}-{i}", order_1.id))
        .bind(if *offset_min == 5 { "order_created" } else { "charging_started" })
        .bind(format!("事件{}说明", i + 1))
        .bind(offset_min)
        .execute(db)
        .await?;
    }
    seed_process(db, alloc, &mut order_1).await?;
    // order_2：card_charge 关联（start_source=card 优先）。
    sqlx::query(
        "INSERT INTO `card_charge` (`charge_order_id`, `card_id`, `port_code`, `paid_cents`, \
         `purchased_minutes`, `max_minutes`, `card_no`, `wallet_after_cents`, `package_json`) \
         VALUES (?, ?, 'pc', 500, 60, 600, ?, 9500, CAST('{}' AS JSON))",
    )
    .bind(order_2.id as i64)
    .bind(alloc.next_id().await? as i64)
    .bind(format!("card-{}", order_2.id))
    .execute(db)
    .await?;
    let refund_id = alloc.next_id().await?;
    sqlx::query(
        "INSERT INTO `refund_record` (`id`, `refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `next_attempt_at`, `created_month`) \
         VALUES (?, ?, ?, ?, 'charge', ?, 300, '测试退款', 'pending', 'manual_review', UTC_TIMESTAMP(3), ?)",
    )
    .bind(refund_id as i64)
    .bind(format!("RF{refund_id:016x}"))
    .bind(order_1.payment_id as i64)
    .bind(user_id as i64)
    .bind(order_1.id as i64)
    .bind(month())
    .execute(db)
    .await?;

    // 充值支付单（3 小时前创建：payment-orders 时间过滤应排除）。
    let pay_3 = alloc.next_id().await?;
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, \
         `total_cents`, `paid_cents`, `status`, `created_at`, `created_month`) \
         VALUES (?, ?, 'wallet_recharge', ?, ?, 'wechat', 500, 0, 'initiated', ?, ?)",
    )
    .bind(pay_3 as i64)
    .bind(format!("P{pay_3}"))
    .bind(user_id as i64)
    .bind(user_id as i64)
    .bind(now - chrono::Duration::hours(3))
    .bind(month())
    .execute(db)
    .await?;

    Ok(Seed {
        station_id,
        user_id,
        rule_id,
        order_1,
        order_2,
        order_3,
        pay_3,
        refund_id,
    })
}

/// 尾部自清理：按主键逆序删除本批种子。
async fn cleanup(db: &sqlx::MySqlPool, seed: &Seed) {
    let orders = [&seed.order_1, &seed.order_2, &seed.order_3];
    for o in orders {
        let _ = sqlx::query("DELETE FROM `charge_process` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_event_log` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_fee_receipt` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_order_pricing` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_payment_intent` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `card_charge` WHERE `charge_order_id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `refund_record` WHERE `payment_order_id` = ?")
            .bind(o.payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
            .bind(o.id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
            .bind(o.payment_id as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
        .bind(seed.pay_3 as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
        .bind(seed.user_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(seed.rule_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
        .bind(seed.station_id as i64)
        .execute(db)
        .await;
}

/// 在响应 items 中找本批订单（按 order_id）。
fn find_item(body: &Value, order_id: u64) -> &Value {
    body["data"]["items"]
        .as_array()
        .expect("items 应为数组")
        .iter()
        .find(|it| it["order_id"].as_u64() == Some(order_id))
        .unwrap_or_else(|| panic!("列表应包含订单 {order_id}: {}", body))
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn admin_orders_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let (token, admin_id) = login_super(&router, &db, &alloc).await;
    let seed = seed_all(db.as_ref(), alloc.as_ref())
        .await
        .expect("种子应成功");

    // ---- 列表：按本批次站点过滤（station_id 为本轮专属，隔离库内历史残留）----
    let path = format!(
        "/api/v1/admin/orders?station_id={}&page=1&page_size=100",
        seed.station_id
    );
    let (status, body) = req(&router, "GET", &path, None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "列表应成功: {body}");
    assert_eq!(body["code"], 0);
    // 站点过滤命中带站点意图的种子单（order_1/order_2）；order_3 刻意无
    // 站点意图（投影 station_id 为空），不经本断言，由 order_no 精确筛选覆盖。
    assert!(
        body["data"]["total"].as_i64().unwrap() >= 2,
        "total 应不少于带站点种子数: {body}"
    );
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["page_size"], 100);
    let items = body["data"]["items"].as_array().unwrap();
    for id in [seed.order_1.id, seed.order_2.id] {
        assert!(
            items.iter().any(|it| it["order_id"].as_u64() == Some(id)),
            "列表应包含订单 {id}"
        );
    }

    // ---- 列表：order_no 精确筛选与整行字段契约 ----
    let path = format!("/api/v1/admin/orders?order_no={}", seed.order_1.order_no);
    let (status, body) = req(&router, "GET", &path, None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["total"], 1);
    let row = &body["data"]["items"][0];
    assert_eq!(row["order_id"], seed.order_1.id);
    assert_eq!(row["order_no"], seed.order_1.order_no);
    assert_eq!(
        row["user_id"],
        seed.user_id.to_string(),
        "user_id 应为字符串"
    );
    assert_eq!(row["device_id"], seed.order_1.device_id);
    assert_eq!(row["port_no"], 1);
    assert_eq!(row["station_id"], seed.station_id);
    assert_eq!(row["station_name"], "订单查询测试站");
    assert_eq!(row["status"], "completed");
    assert_eq!(row["business_status"], "completed");
    assert_eq!(row["payment_status"], "paid");
    assert_eq!(row["start_source"], "payment");
    assert_eq!(row["duration_seconds"], 600);
    assert_eq!(row["meter_kwh"], "0.2000", "电量应保持 DECIMAL 文本");
    assert_eq!(row["electric_fee_cents"], 10);
    assert_eq!(row["service_fee_cents"], 6);
    assert_eq!(row["total_fee_cents"], 16);
    assert_eq!(
        row["refund_status"], "processing",
        "待审退款应使进度为 processing"
    );
    assert_eq!(row["payment_order_id"], seed.order_1.payment_id);
    assert_eq!(
        row["payment_order_no"],
        format!("P{}", seed.order_1.payment_id)
    );
    assert_eq!(row["payment_order_status"], "paid");
    assert_eq!(row["paid_cents"], 1000);
    assert_eq!(row["refunded_cents"], 0);
    assert_eq!(row["failure_reason"], Value::Null);
    assert_eq!(row["refund_applicant_id"], Value::Null, "列表不回填申请人");
    assert_eq!(row["billing"], Value::Null);
    assert_eq!(row["selected_package"], Value::Null);
    assert_eq!(
        row["selected_scheme_name"], "订单查询测试方案",
        "列表回填冻结名称"
    );
    assert_eq!(row["selected_package_name"], "1元");
    assert!(row.get("live").is_none(), "未接线的实时估算不应输出 live");
    assert!(
        row.get("live_unavailable").is_none(),
        "完成单不应有不可用提示"
    );
    assert!(row["created_at"].as_str().is_some());
    assert!(row["started_at"].as_str().is_some());
    assert!(row["ended_at"].as_str().is_some());

    // ---- 列表：各筛选维度 ----
    // 设备号。
    let path = format!("/api/v1/admin/orders?device_id={}", seed.order_2.device_id);
    let (status, body) = req(&router, "GET", &path, None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["items"][0]["order_id"], seed.order_2.id);
    // 业务状态 + live_unavailable（充电中单）。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/orders?business_status=charging&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let row = find_item(&body, seed.order_2.id);
    assert_eq!(
        row["start_source"], "card",
        "card_charge 关联应优先于支付方式"
    );
    assert_eq!(
        row["live_unavailable"], "设备计量暂不可读取",
        "充电中且已开始的单应有实时不可用提示"
    );
    assert!(row["meter_kwh"].is_null(), "充电中单尚无终读数");
    // 支付状态（未支付单）。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/orders?payment_status=pending&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let row = find_item(&body, seed.order_3.id);
    assert_eq!(row["start_source"], Value::Null, "无有效支付单时来源为空");
    assert_eq!(row["station_id"], Value::Null, "无意图时站点为空");
    assert_eq!(row["station_name"], Value::Null);
    assert_eq!(row["payment_order_id"], Value::Null);
    assert_eq!(row["refund_status"], "none");
    assert_eq!(row["started_at"], Value::Null);
    assert_eq!(row["duration_seconds"], Value::Null);
    // 启动来源。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/orders?start_source=card&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        find_item(&body, seed.order_2.id)["order_id"],
        seed.order_2.id
    );
    // 站点。
    let (status, body) = req(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/orders?station_id={}&page_size=100",
            seed.station_id
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["order_id"].as_u64())
        .collect();
    assert!(ids.contains(&seed.order_1.id) && ids.contains(&seed.order_2.id));
    assert!(!ids.contains(&seed.order_3.id), "无意图单不落入站点过滤");
    // 内部状态。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/orders?status=charging&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        find_item(&body, seed.order_2.id)["order_id"],
        seed.order_2.id
    );
    // 时间区间（闭区间）：started 回退 created 的双向分支。
    // 下界放宽到 35 分钟前，避免种子写入与过滤求值两次取 now 的毫秒漂移。
    let from = (chrono::Utc::now() - chrono::Duration::minutes(35))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let to = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders?started_from={from}&started_to={to}&page_size=100"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<u64> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["order_id"].as_u64())
        .collect();
    assert!(
        ids.contains(&seed.order_1.id),
        "区间应含 30 分钟前启动单: {ids:?}"
    );
    assert!(ids.contains(&seed.order_2.id));
    assert!(
        !ids.contains(&seed.order_3.id),
        "3 小时前创建且未启动的单应被排除: {ids:?}"
    );

    // ---- 列表：分页 ----
    let (status, body) = req(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/orders?device_id={}&page=1&page_size=1",
            seed.order_1.device_id
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);

    // ---- 列表：入参校验（400/1005） ----
    for (query, message) in [
        ("business_status=bogus", "业务状态、支付状态或启动来源无效"),
        ("payment_status=bogus", "业务状态、支付状态或启动来源无效"),
        ("start_source=bogus", "业务状态、支付状态或启动来源无效"),
        ("station_id=0", "站点 ID 无效"),
        ("station_id=abc", "站点 ID 无效"),
        ("started_from=not-a-time", "时间格式须为 ISO 8601"),
        (
            "order_no=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "订单号或设备号过长",
        ),
        (
            "keyword=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "关键词过长或状态无效",
        ),
        ("status=bogus", "关键词过长或状态无效"),
    ] {
        let (status, body) = req(
            &router,
            "GET",
            &format!("/api/v1/admin/orders?{query}"),
            None,
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["code"], 1005, "{query}: {body}");
        assert_eq!(body["message"], message, "{query}: {body}");
    }
    // 起止倒置。
    let bad_from = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let bad_to = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders?started_from={bad_from}&started_to={bad_to}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "开始时间不能晚于结束时间");
    // 分页越界。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/orders?page_size=101",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "分页参数无效：page_size 为 1–100");

    // ---- 无令牌：401 ----
    let (status, body) = req(&router, "GET", "/api/v1/admin/orders", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // ---- 详情：完整契约 ----
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}", seed.order_1.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = &body["data"];
    assert_eq!(detail["order_id"], seed.order_1.id);
    assert_eq!(detail["selected_scheme_name"], "订单查询测试方案");
    assert_eq!(detail["selected_package_name"], "1元");
    assert_eq!(
        detail["selected_package"]["offer"]["name"], "1元",
        "冻结套餐 offer 应原样输出: {}",
        detail["selected_package"]
    );
    assert_eq!(detail["selected_package"]["offer"]["price_cents"], 1000);
    assert_eq!(
        detail["selected_package"]["billing_mode"], "server_energy",
        "billing_mode 取自冻结规则的计费模式"
    );
    assert_eq!(detail["selected_package"]["rule_version"], 1);
    assert_eq!(
        detail["selected_package"]["scheme"]["name"],
        "订单查询测试方案"
    );
    assert!(
        detail["selected_package"].get("card").is_none(),
        "非刷卡单不应有 card 累计"
    );
    assert_eq!(
        detail["billing"]["calculation_no"],
        format!("QB{}", seed.order_1.id),
        "详情应回填计费单号"
    );
    assert_eq!(
        detail["refund_applicant_id"],
        admin_id.to_string(),
        "持有 order.refund.create 的操作人应回填申请人"
    );
    assert_eq!(detail["meter_kwh"], "0.2000");
    assert_eq!(detail["user_id"], seed.user_id.to_string());

    // 充电中详情：live_unavailable；card 累计挂到 selected_package.card。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}", seed.order_2.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = &body["data"];
    assert_eq!(detail["live_unavailable"], "设备计量暂不可读取");
    assert!(detail.get("live").is_none());
    assert_eq!(
        detail["selected_package"],
        Value::Null,
        "无快照单套餐为 null"
    );
    assert_eq!(
        detail["billing"]["calculation_no"],
        Value::Null,
        "未计费单计费单号为 null"
    );

    // 缺失与非法 ID。
    let missing = alloc.next_id().await.unwrap();
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{missing}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], 1004);
    assert_eq!(body["message"], "记录不存在或已删除");
    let (status, body) = req(&router, "GET", "/api/v1/admin/orders/0", None, Some(&token)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "ID 必须为正整数");

    // ---- 时间线 ----
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/timeline", seed.order_1.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["order_id"], seed.order_1.id);
    let events = body["data"]["timeline"].as_array().unwrap();
    assert_eq!(events.len(), 2, "应返回两条种子事件: {body}");
    assert_eq!(events[0]["event_id"], format!("evt-{}-0", seed.order_1.id));
    assert_eq!(events[0]["event"], "order_created");
    assert_eq!(events[0]["actor"], "system");
    assert_eq!(events[0]["detail"], "事件1说明");
    assert!(events[0]["at"].as_str().is_some());
    assert_eq!(events[1]["event"], "charging_started", "按发生时间排序");
    // 空时间线（订单存在但无事件）。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/timeline", seed.order_3.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["order_id"], seed.order_3.id);
    assert_eq!(
        body["data"]["timeline"].as_array().unwrap().len(),
        0,
        "无事件返回空数组"
    );
    // 缺失订单不是空时间线。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{missing}/timeline"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], 1004);

    // ---- 过程采样：游标分页 ----
    let ids = &seed.order_1.process_ids;
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/process", seed.order_1.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let points = body["data"]["items"].as_array().unwrap();
    assert_eq!(points.len(), 3, "默认 limit=1000 应取全部: {body}");
    assert_eq!(body["data"]["next_after_id"], Value::Null, "末页游标为空");
    assert_eq!(points[0]["id"], ids[0]);
    assert_eq!(points[0]["power_w"], 150.0, "0.1W 原始值应换算为 W");
    assert_eq!(points[0]["charged_kwh"], 0.05, "mWh 原始值应换算为 kWh");
    assert_eq!(points[0]["charged_seconds"], 60);
    assert_eq!(points[0]["remaining_seconds"], 540);
    assert_eq!(points[0]["signal_strength"], 20);
    assert_eq!(points[0]["port_status"], 1);
    assert_eq!(points[0]["voltage_v"], 220);
    assert_eq!(points[0]["temperature_c"], 25);
    assert_eq!(points[0]["device_status"], 2);
    assert!(points[0]["ts"].as_str().is_some());
    // 游标 + limit=1：第二行，游标指向该行 ID。
    let (status, body) = req(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/orders/{}/process?after_id={}&limit=1",
            seed.order_1.id, ids[0]
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let points = body["data"]["items"].as_array().unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0]["id"], ids[1]);
    assert_eq!(
        body["data"]["next_after_id"], ids[1],
        "还有剩余行时返回该行 ID"
    );
    // limit=2：两行 + 游标。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/process?limit=2", seed.order_1.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["next_after_id"], ids[1]);
    // 游标越过末行：空数组。
    let (status, body) = req(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/orders/{}/process?after_id={}",
            seed.order_1.id, ids[2]
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["data"]["next_after_id"], Value::Null);
    // 无过程数据的订单：空数组。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/process", seed.order_3.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 0);
    // 缺失订单。
    let (status, _) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{missing}/process"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // 参数校验。
    for query in [
        "limit=0",
        "limit=2001",
        "after_id=abc",
        "limit=abc",
        "after_id=",
    ] {
        let (status, body) = req(
            &router,
            "GET",
            &format!("/api/v1/admin/orders/{}/process?{query}", seed.order_1.id),
            None,
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["code"], 1005, "{query}: {body}");
    }
    // 上边界 limit=2000 合法。
    let (status, body) = req(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/orders/{}/process?limit=2000",
            seed.order_3.id
        ),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // ---- 订单关联退款单 ----
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/refunds", seed.order_1.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["order_id"], seed.order_1.id);
    assert_eq!(body["data"]["total"], 1);
    let refund = &body["data"]["items"][0];
    assert_eq!(refund["refund_no"], format!("RF{:016x}", seed.refund_id));
    assert_eq!(refund["status"], "pending");
    assert_eq!(refund["refund_cents"], 300);
    assert_eq!(refund["execution_policy"], "manual_review");
    assert_eq!(
        refund["review_first_signer"],
        Value::Null,
        "未经复核审核字段为空"
    );
    assert!(refund["created_at"].as_str().is_some());
    // 无退款订单：空数组。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{}/refunds", seed.order_3.id),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 0);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 0);
    // 缺失订单。
    let (status, _) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/orders/{missing}/refunds"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // ---- 支付单列表 ----
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/payment-orders?page=1&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["total"].as_i64().unwrap() >= 3);
    let items = body["data"]["items"].as_array().unwrap();
    let find_pay = |no: u64| -> Value {
        items
            .iter()
            .find(|it| it["payment_order_id"].as_u64() == Some(no))
            .cloned()
            .unwrap_or_else(|| panic!("应包含支付单 {no}: {body}"))
    };
    let pay1 = find_pay(seed.order_1.payment_id);
    assert_eq!(pay1["order_no"], format!("P{}", seed.order_1.payment_id));
    assert_eq!(
        pay1["user_id"],
        seed.user_id.to_string(),
        "user_id 应为字符串"
    );
    assert_eq!(pay1["biz_type"], "charge");
    assert_eq!(pay1["pay_method"], "wechat");
    assert_eq!(pay1["status"], "paid");
    assert_eq!(pay1["payment_status"], "paid");
    assert_eq!(pay1["total_cents"], 1000);
    assert_eq!(pay1["paid_cents"], 1000);
    assert_eq!(pay1["refunded_cents"], 0);
    assert!(pay1["paid_at"].as_str().is_some());
    assert_eq!(
        pay1["charge_order_id"], seed.order_1.id,
        "充值外支付单应关联充电单"
    );
    assert_eq!(pay1["charge_order_no"], seed.order_1.order_no);
    let pay2 = find_pay(seed.order_2.payment_id);
    assert_eq!(pay2["pay_method"], "balance");
    assert_eq!(pay2["charge_order_no"], seed.order_2.order_no);
    let pay3 = find_pay(seed.pay_3);
    assert_eq!(pay3["biz_type"], "wallet_recharge");
    assert_eq!(
        pay3["payment_status"], "pending",
        "未到账充值单金额状态为 pending"
    );
    assert_eq!(pay3["charge_order_id"], Value::Null, "充值单不关联充电单");
    assert_eq!(pay3["charge_order_no"], Value::Null);

    // 筛选。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/payment-orders?order_no=P{}", seed.pay_3),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["items"][0]["payment_order_id"], seed.pay_3);
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/payment-orders?biz_type=wallet_recharge&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["payment_order_id"].as_u64())
        .collect();
    assert!(ids.contains(&seed.pay_3));
    assert!(!ids.contains(&seed.order_1.payment_id));
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/payment-orders?payment_status=pending&pay_method=wechat&page_size=100",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["payment_order_id"].as_u64())
        .collect();
    assert!(ids.contains(&seed.pay_3), "组合筛选应命中充值单: {ids:?}");
    // 时间区间。
    let from = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/payment-orders?created_from={from}&page_size=100"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<u64> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["payment_order_id"].as_u64())
        .collect();
    assert!(
        !ids.contains(&seed.pay_3),
        "3 小时前创建的充值单应被排除: {ids:?}"
    );
    assert!(ids.contains(&seed.order_1.payment_id));

    // 入参校验。
    for (query, message) in [
        ("biz_type=bogus", "支付单号、用途、支付方式或支付状态无效"),
        (
            "pay_method=coupon",
            "支付单号、用途、支付方式或支付状态无效",
        ),
        (
            "payment_status=bogus",
            "支付单号、用途、支付方式或支付状态无效",
        ),
        ("status=active", "关键词过长或状态无效"),
        (
            "created_from=not-a-time",
            "时间格式须为 ISO 8601，且在有效日期范围内",
        ),
    ] {
        let (status, body) = req(
            &router,
            "GET",
            &format!("/api/v1/admin/payment-orders?{query}"),
            None,
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["message"], message, "{query}: {body}");
    }
    let (status, _) = req(&router, "GET", "/api/v1/admin/payment-orders", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    cleanup(db.as_ref(), &seed).await;
}
