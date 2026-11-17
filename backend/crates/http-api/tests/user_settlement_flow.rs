//! 集成测试在持锁场景下跨 await；已配置 clippy 豁免。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 用户端账单与发票流集成测试：计费入账同事务签发账单（生产接线验证）、
//! 列表/详情/已读幂等、开票申请幂等与一单一份、越权 404（与对外 API 契约一致）。
//! 数据准备用 SQL 直插 + settlement 的 process_job 真实入账（断言账单由
//! 计费事务自动签发，而非测试手动补签）。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use support::{TestApp, rand_suffix, req};

/// 资金批次互斥锁（共享 dev 库的 flow 测试串行）。
/// 合法 UUID v4（发票申请的幂等请求号须通过服务端 uuid 校验）。
fn rand_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
}

async fn login_user(router: &axum::Router, tag: &str) -> (String, u64) {
    let (status, body) = req(
        router,
        "POST",
        "/api/v1/user/auth/login",
        Some(json!({"code": format!("dev:{tag}")})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "用户登录失败: {body}");
    let token = body["data"]["jwt"].as_str().unwrap().to_string();
    let uid: u64 = body["data"]["user_id"]
        .as_str()
        .expect("user_id 字符串")
        .parse()
        .expect("user_id 可解析");
    (token, uid)
}

/// 测试方案：amount 套餐 100 分，电价 50+30 分/kWh（种子与快照共用同一份定义）。
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

/// 种子：一份电量计费方案规则（站名固定，尾部按名清理）。
async fn seed_rule(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> u64 {
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '账单测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `version`, `status`, `spec_json`) VALUES (?, '账单测试规则', ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    rule_id
}

/// 计费批次种子的关联记录，供尾部自清理。
struct Seed {
    charge_order_id: u64,
    payment_id: u64,
    device_id: String,
}

/// 种子：一笔完成充电的订单（paid 支付单与 completed 订单，附计费快照、
/// 结束计量与计费任务）。用户为已登录的真实用户（不插 user 行）。
/// 快照形态与真实 checkout 冻结一致：rule.spec 为套餐引擎规格，offer 携带 station_id。
#[allow(clippy::too_many_arguments)]
async fn seed_completed_charge(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    station_id: u64,
    rule_id: u64,
    user_id: u64,
    paid_cents: i64,
    charged_wh: u32,
) -> Seed {
    let payment_id = alloc.next_id().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-us-{}", rand_suffix());
    let merchant_order_no = format!("P{payment_id}");
    let order_no = format!(
        "C{}{}01",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        device_id
    );
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'charge', ?, ?, 'wechat', ?, ?, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_id as i64).bind(&merchant_order_no).bind(charge_order_id as i64)
        .bind(user_id as i64).bind(paid_cents).bind(paid_cents).bind(month)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `started_at`, `ended_at`, `charged_kwh`, `charged_seconds`, `created_month`) VALUES (?, ?, ?, ?, 1, 'pc', ?, 'completed', 'paid', DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE), UTC_TIMESTAMP(3), 0.2000, 600, ?)")
        .bind(charge_order_id as i64).bind(&order_no).bind(user_id as i64).bind(&device_id)
        .bind(payment_id as i64).bind(month)
        .execute(db).await.unwrap();
    // 快照经 domain 真实 API 生成（与 checkout 冻结链一致）。
    let snap: chargepilot_domain::charging::scheme::SpecSnapshot =
        serde_json::from_value(json!({"scheme": test_scheme()})).expect("方案应可解析");
    let scheme = snap.scheme.expect("方案存在");
    let package = scheme.package(1).expect("套餐存在");
    let engine_spec = scheme.spec_for(&package);
    let estimate = chargepilot_domain::payment::intent_estimate(&scheme, &package, paid_cents)
        .map_err(|e| anyhow::anyhow!(format!("{e:?}")))
        .expect("预估应成功");
    let offer = chargepilot_domain::charging::settle::Offer {
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
        rule: chargepilot_domain::charging::actual::Rule {
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
    Seed {
        charge_order_id,
        payment_id,
        device_id,
    }
}

/// 尾部自清理：按主键删除本批种子（流水与事件保留供追溯）。
async fn cleanup(db: &sqlx::MySqlPool, seeds: &[Seed]) {
    for s in seeds {
        let _ = sqlx::query(
            "DELETE br FROM `charge_bill_read` br \
             JOIN `charge_bill` b ON b.`id` = br.`bill_id` WHERE b.`charge_order_id` = ?",
        )
        .bind(s.charge_order_id as i64)
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `charge_bill` WHERE `charge_order_id` = ?")
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        for table in [
            "charge_fee_receipt",
            "charge_billing_job",
            "charge_end_receipt",
            "charge_order_pricing",
            "charge_order",
        ] {
            let _ = sqlx::query(&format!(
                "DELETE FROM `{table}` WHERE `charge_order_id` = ?"
            ))
            .bind(s.charge_order_id as i64)
            .execute(db)
            .await;
        }
        let _ = sqlx::query("DELETE FROM `refund_record` WHERE `payment_order_id` = ?")
            .bind(s.payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
            .bind(s.payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
            .bind(&s.device_id)
            .execute(db)
            .await;
    }
}

/// 发票申请自清理（审核记录先行，对齐 admin_billing_flow 惯例）。
async fn cleanup_invoices(db: &sqlx::MySqlPool, uid: u64) {
    let _ = sqlx::query(
        "DELETE r FROM `invoice_admin_review` r \
         JOIN `invoice_request` i ON i.`id` = r.`invoice_request_id` WHERE i.`user_id` = ?",
    )
    .bind(uid as i64)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `invoice_request` WHERE `user_id` = ?")
        .bind(uid as i64)
        .execute(db)
        .await;
}

/// 场景一（账单）：计费入账事务自动签发账单 → 列表/详情字段齐 → 已读幂等
/// （两次 read，read_at 不变）→ 越权 404（detail 与 read 同一致）。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn bill_lifecycle_read_idempotent_and_forbidden() {
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let (token_a, uid_a) = login_user(&router, &format!("us-a-{}", rand_suffix())).await;
    let (token_b, _) = login_user(&router, &format!("us-b-{}", rand_suffix())).await;
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = seed_rule(db.as_ref(), &alloc).await;
    // 预付 100 分，实耗 200Wh（实费 16 分）→ 账单 total 16 分，余款 84 分退款。
    let seed =
        seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, uid_a, 100, 200).await;
    let handled =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed.charge_order_id)
            .await
            .expect("计费任务应成功");
    assert!(handled, "任务应完成");

    // 账单由计费入账事务自动签发（生产接线）：恰一行，金额与预付推导正确。
    let bill: (String, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT CAST(`bill_no` AS CHAR), `electric_cents`, `service_cents`, `total_cents`, \
         `prepaid_cents`, `refund_cents`, `shortfall_cents` FROM `charge_bill` \
         WHERE `charge_order_id` = ?",
    )
    .bind(seed.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .expect("计费事务应已签发账单");
    let (bill_no, electric, service, total, prepaid, refund, shortfall) = bill;
    assert_eq!((electric, service, total), (10, 6, 16));
    // refund_cents 是渠道已退累计（契约约定）：余款 84 分此时尚为 pending 在途，
    // 尚未执行渠道退款，故为 0；封顶入账无缺口。
    assert_eq!((prepaid, refund, shortfall), (100, 0, 0));
    assert_eq!(bill_no, format!("BILL{:020}", seed.charge_order_id));
    let (id,): (String,) =
        sqlx::query_as("SELECT CAST(`id` AS CHAR) FROM `charge_bill` WHERE `charge_order_id` = ?")
            .bind(seed.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    let bill_id: u64 = id.parse().unwrap();

    // 列表：一条记录，字段齐（id/user_id 字符串、时间 +00:00、read 未读）。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/user/bills?page=1&page_size=20",
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "账单列表失败: {body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["page_size"], 20);
    let item = &body["data"]["items"][0];
    assert_eq!(item["id"], id.as_str(), "账单 ID 输出十进制字符串");
    assert_eq!(item["bill_no"], bill_no.as_str());
    assert_eq!(item["total_cents"], 16);
    assert_eq!(item["electric_cents"], 10);
    assert_eq!(item["service_cents"], 6);
    assert_eq!(item["prepaid_cents"], 100);
    assert_eq!(item["shortfall_cents"], 0);
    assert_eq!(item["status"], "issued");
    assert_eq!(item["read"], false, "未读账单 read 布尔为 false");
    let issued_at = item["issued_at"].as_str().expect("issued_at 字符串");
    assert!(
        issued_at.contains('T') && issued_at.contains("+00:00"),
        "issued_at 输出 {issued_at} 应为 +00:00 时间"
    );
    assert!(
        item["user_id"].as_str().is_some(),
        "user_id 输出十进制字符串"
    );
    assert!(
        item["charged_kwh"].as_str().is_some(),
        "charged_kwh 输出十进制文本"
    );

    // 详情：字段与列表一致；他人账单编号不存在（越权与缺失一致 404）。
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/user/bills/{bill_id}"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "账单详情失败: {body}");
    assert_eq!(body["data"]["id"], id.as_str());
    assert_eq!(body["data"]["total_cents"], 16);

    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/user/bills/{bill_id}"),
        None,
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "越权详情应 404: {body}");
    assert_eq!(body["code"], 1004);

    // 已读幂等：两次 POST /read 均成功，read_at 不变。
    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/user/bills/{bill_id}/read"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "已读标记失败: {body}");
    assert_eq!(body["data"]["id"], id.as_str());
    assert_eq!(body["data"]["read"], true);
    let (read_at_first,): (String,) = sqlx::query_as(
        "SELECT DATE_FORMAT(`read_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') \
         FROM `charge_bill_read` WHERE `bill_id` = ? AND `user_id` = ?",
    )
    .bind(bill_id as i64)
    .bind(uid_a as i64)
    .fetch_one(db.as_ref())
    .await
    .expect("已读记录应存在");
    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/user/bills/{bill_id}/read"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重复已读不应报错: {body}");
    let (read_at_second,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM `charge_bill_read` \
         WHERE `bill_id` = ? AND `user_id` = ? \
         AND DATE_FORMAT(`read_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') = ?",
    )
    .bind(bill_id as i64)
    .bind(uid_a as i64)
    .bind(&read_at_first)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(read_at_second, 1, "重复已读 read_at 不变");

    // 列表已读状态翻转；越权 read 同 404。
    let (status, body) = req(&router, "GET", "/api/v1/user/bills", None, Some(&token_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["items"][0]["read"], true,
        "已读后列表 read 翻转"
    );

    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/user/bills/{bill_id}/read"),
        None,
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "越权已读应 404: {body}");
    assert_eq!(body["code"], 1004);

    // 无凭据 401（鉴权先行）。
    let (status, _) = req(&router, "GET", "/api/v1/user/bills", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    cleanup(db.as_ref(), &[seed]).await;
    let _ = sqlx::query("DELETE FROM `station` WHERE `name` = '账单测试站'")
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(rule_id as i64)
        .execute(db.as_ref())
        .await;
}

/// 场景二（发票）：refunding 订单申请开票 → 重复同 UUID 幂等复用 →
/// 换请求号同订单 409 → my 列表可见 → 他人订单 404。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn invoice_apply_idempotent_conflict_and_my_list() {
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let (token_a, uid_a) = login_user(&router, &format!("inv-a-{}", rand_suffix())).await;
    let (token_b, _uid_b) = login_user(&router, &format!("inv-b-{}", rand_suffix())).await;
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = seed_rule(db.as_ref(), &alloc).await;
    let seed =
        seed_completed_charge(db.as_ref(), &alloc, station_id, rule_id, uid_a, 100, 200).await;
    let handled =
        chargepilot_mod_settlement::billing::process_job(db.as_ref(), seed.charge_order_id)
            .await
            .expect("计费任务应成功");
    assert!(handled, "任务应完成");
    // 订单在余款退款登记后进入 refunding（可开票状态之一）。
    let (order_status,): (String,) =
        sqlx::query_as("SELECT CAST(`status` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(seed.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(order_status, "refunding", "种子应落在可开票状态");
    let (order_no,): (String,) =
        sqlx::query_as("SELECT `order_no` FROM `charge_order` WHERE `id` = ?")
            .bind(seed.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();

    let apply_body = |request_id: &str, order_no: &str| {
        json!({
            "request_id": request_id,
            "order_no": order_no,
            "invoice_type": "normal",
            "title": "账单测试抬头",
            "tax_no": "91330100MA27X8XXXX",
            "email": "bill@example.com",
        })
    };
    let request_id = rand_uuid();
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/invoice/apply",
        Some(apply_body(&request_id, &order_no)),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "开票申请失败: {body}");
    assert_eq!(body["code"], 0);
    let invoice_no = body["data"]["invoice_no"].as_str().unwrap().to_string();
    let apply_id = body["data"]["id"].as_i64().unwrap();
    assert_eq!(
        invoice_no,
        format!("INV{}", request_id.replace('-', "").to_uppercase()),
        "发票号由请求号派生"
    );
    assert_eq!(body["data"]["review_status"], "pending");
    assert_eq!(body["data"]["total_cents"], 16, "开票额取回执确定总额");

    // 同请求号同内容重放：幂等复用同一申请。
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/invoice/apply",
        Some(apply_body(&request_id, &order_no)),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放开票申请应幂等: {body}");
    assert_eq!(body["data"]["id"], apply_id, "重放应复用同一申请 ID");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `invoice_request` WHERE `user_id` = ?")
            .bind(uid_a as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(count, 1, "重放不新增申请行");

    // 换请求号同订单：一单一份，409 冲突。
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/invoice/apply",
        Some(apply_body(&rand_uuid(), &order_no)),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "换号重复申请应 409: {body}");
    assert_eq!(body["code"], 2009);

    // 同请求号不同内容：内容冲突，409。
    let mut mismatched = apply_body(&request_id, &order_no);
    mismatched["title"] = json!("另一个抬头");
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/invoice/apply",
        Some(mismatched),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "同号不同内容应 409: {body}");

    // 他人订单：归属不符 404。
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/invoice/apply",
        Some(apply_body(&rand_uuid(), &order_no)),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "他人订单申请应 404: {body}");
    assert_eq!(body["code"], 1004);

    // my 列表：一条 pending 申请，字段齐。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/user/invoice/my?page=1&page_size=20",
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "我的发票失败: {body}");
    assert_eq!(body["data"]["total"], 1);
    let row = &body["data"]["items"][0];
    assert_eq!(row["invoice_no"], invoice_no.as_str());
    assert_eq!(row["review_status"], "pending");
    assert_eq!(row["invoice_type"], "normal");
    assert_eq!(row["title"], "账单测试抬头");
    assert_eq!(row["total_cents"], 16);
    assert_eq!(row["biz_type"], "charge");
    assert!(
        row["created_at"].as_str().is_some(),
        "created_at 时间字符串"
    );
    // B 的列表为空（数据归属隔离）。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/user/invoice/my",
        None,
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 0);

    cleanup_invoices(db.as_ref(), uid_a).await;
    cleanup(db.as_ref(), &[seed]).await;
    let _ = sqlx::query("DELETE FROM `station` WHERE `name` = '账单测试站'")
        .execute(db.as_ref())
        .await;
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(rule_id as i64)
        .execute(db.as_ref())
        .await;
}
