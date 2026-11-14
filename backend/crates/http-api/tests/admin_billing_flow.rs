#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 后台财务端点主缝集成测试：发票列表与双审、计量核实 propose→decide 全流程
//! 与权限分支、人工退款列表/申请/双审/重试（路径、响应形状与权限码对齐旧版契约）。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use support::{TestApp, flow_lock, login_admin, rand_suffix, req_str};

/// 测试方案：套餐 100 分，电价 50+30 分/kWh（种子与快照共用同一份定义）。
fn test_scheme() -> Value {
    json!({
        "name": "财务端点测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    })
}

struct AdminSeed {
    username: String,
    admin_id: u64,
    role_id: u64,
}

/// SQL 直建挂内置 customer_finance 角色（id=4，迁移已种入全部财务权限码）的管理员。
async fn seed_finance_admin(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    suffix: &str,
) -> AdminSeed {
    let username = format!("fin-{suffix}");
    let admin_id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("finance-pass-123").unwrap();
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 4)",
    )
    .bind(admin_id)
    .bind(&username)
    .bind(hash)
    .execute(db)
    .await
    .unwrap();
    AdminSeed {
        username,
        admin_id,
        role_id: 4,
    }
}

/// SQL 直建指定权限集合的管理员（角色自定义）。
async fn seed_admin(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    suffix: &str,
    permissions: &[&str],
) -> AdminSeed {
    let username = format!("fin-{suffix}");
    let role_id = alloc.next_id().await.unwrap();
    let admin_id = alloc.next_id().await.unwrap();
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '财务测试角色', 0)",
    )
    .bind(role_id)
    .bind(format!("fin_{suffix}"))
    .execute(db)
    .await
    .unwrap();
    for p in permissions {
        sqlx::query(
            "INSERT INTO `role_permission` (`role_id`, `permission_id`) \
             SELECT ?, `id` FROM `permission` WHERE `code` = ?",
        )
        .bind(role_id)
        .bind(p)
        .execute(db)
        .await
        .unwrap();
    }
    let hash = chargepilot_platform::password::hash("finance-pass-123").unwrap();
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)",
    )
    .bind(admin_id)
    .bind(&username)
    .bind(hash)
    .bind(role_id)
    .execute(db)
    .await
    .unwrap();
    AdminSeed {
        username,
        admin_id,
        role_id,
    }
}

/// 种子：一笔完成充电的订单（paid 100 分 + 计量 + 计费任务），形态与 settlement_flow 一致。
/// 返回 (charge_order_id, payment_id, user_id, rule_id, station_id, device_id)。
#[allow(clippy::type_complexity)]
async fn seed_completed_charge(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    charged_wh: u32,
) -> (u64, u64, u64, u64, u64, String) {
    let station_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    let payment_id = alloc.next_id().await.unwrap();
    let charge_order_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-fin-{}", rand_suffix());
    let order_no = format!(
        "C{}{}01",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        device_id
    );
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '财务端点测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `version`, `status`, `spec_json`) VALUES (?, '财务端点测试规则', ?, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '财务测试用户', 'active')")
        .bind(user_id as i64).bind(format!("fin-{user_id}")).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'charge', ?, ?, 'wechat', 100, 100, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_id as i64).bind(format!("P{payment_id}")).bind(charge_order_id as i64)
        .bind(user_id as i64).bind(month)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `started_at`, `ended_at`, `charged_kwh`, `charged_seconds`, `created_month`) VALUES (?, ?, ?, ?, 1, 'pc', ?, 'completed', 'paid', DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE), UTC_TIMESTAMP(3), 0.2000, 600, ?)")
        .bind(charge_order_id as i64).bind(&order_no).bind(user_id as i64).bind(&device_id)
        .bind(payment_id as i64).bind(month)
        .execute(db).await.unwrap();
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
    sqlx::query("INSERT INTO `charge_order_pricing` (`charge_order_id`, `payment_intent_id`, `user_id`, `port_code`, `pricing_snapshot`) VALUES (?, ?, ?, 'pc', CAST(? AS JSON))")
        .bind(charge_order_id as i64).bind(format!("int-{charge_order_id}"))
        .bind(user_id as i64).bind(serde_json::to_string(&snapshot).unwrap())
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
    (
        charge_order_id,
        payment_id,
        user_id,
        rule_id,
        station_id,
        device_id,
    )
}

/// 尾部自清理（订单/支付/发票/审核/审计按主键）。
#[allow(clippy::too_many_arguments)]
async fn cleanup(
    db: &sqlx::MySqlPool,
    charge_seeds: &[(u64, u64, u64, u64, u64, String)],
    invoice_ids: &[u64],
    admin_seeds: &[AdminSeed],
) {
    for (order_id, payment_id, user_id, rule_id, station_id, device_id) in charge_seeds {
        for table in [
            "manual_fee_review",
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
        let _ = sqlx::query("DELETE x FROM `refund_rejection` x \
             JOIN `refund_record` r ON r.`id` = x.`refund_record_id` WHERE r.`payment_order_id` = ?")
            .bind(*payment_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE x FROM `refund_review` x \
             JOIN `refund_record` r ON r.`id` = x.`refund_record_id` WHERE r.`payment_order_id` = ?")
            .bind(*payment_id as i64)
            .execute(db)
            .await;
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
        let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
            .bind(*station_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
            .bind(device_id)
            .execute(db)
            .await;
    }
    for invoice_id in invoice_ids {
        let _ = sqlx::query("DELETE FROM `invoice_admin_review` WHERE `invoice_request_id` = ?")
            .bind(*invoice_id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `invoice_request` WHERE `id` = ?")
            .bind(*invoice_id as i64)
            .execute(db)
            .await;
    }
    for seed in admin_seeds {
        let _ = sqlx::query("DELETE FROM `admin_user` WHERE `id` = ?")
            .bind(seed.admin_id as i64)
            .execute(db)
            .await;
        // 内置角色（id=4）不动；自定义只读角色随测试删除。
        if seed.role_id != 4 {
            let _ = sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
                .bind(seed.role_id as i64)
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM `role` WHERE `id` = ?")
                .bind(seed.role_id as i64)
                .execute(db)
                .await;
        }
    }
}

/// 种子：一条待审核发票申请（一单一活跃发票的唯一键独立在 biz 维度，
/// 每张申请配独立用户，避免 uk_invoice_active 冲突）。返回 (invoice_id, user_id)。
async fn seed_invoice(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> (u64, u64) {
    let invoice_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '发票测试用户', 'active')")
        .bind(user_id as i64).bind(format!("inv-{user_id}")).execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO `invoice_request` (`id`, `invoice_no`, `user_id`, `biz_type`, `biz_id`, \
         `total_cents`, `invoice_type`, `title`) \
         VALUES (?, ?, ?, 'charge', ?, 400, 'normal', '财务端点测试抬头')",
    )
    .bind(invoice_id as i64)
    .bind(format!("INV-FIN-{invoice_id}"))
    .bind(user_id as i64)
    .bind(user_id as i64)
    .execute(db)
    .await
    .unwrap();
    (invoice_id, user_id)
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn invoice_list_and_review_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let suf = rand_suffix();
    let fin1 = seed_finance_admin(&db, &alloc, &format!("1{suf}")).await;
    let fin2 = seed_finance_admin(&db, &alloc, &format!("2{suf}")).await;
    let reader = seed_admin(&db, &alloc, &format!("r{suf}"), &["finance.read"]).await;
    let (invoice_a, user_a) = seed_invoice(&db, &alloc).await;
    let (invoice_b, user_b) = seed_invoice(&db, &alloc).await;

    let t1 = login_admin(&router, &fin1.username, "finance-pass-123").await;
    let t2 = login_admin(&router, &fin2.username, "finance-pass-123").await;
    let tr = login_admin(&router, &reader.username, "finance-pass-123").await;

    // 1. 未登录 401；无 invoice.review 权限 403（列表 finance.read 可读）。
    let (status, body) =
        req_str(&router, "GET", "/api/v1/admin/billing/invoices", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "未登录应 401: {body}");
    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/invoices",
        None,
        Some(&tr),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "只读账号应能读发票列表: {body}");
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/a.pdf"}"#),
        Some(&tr),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无审核权限应 403: {body}");
    assert_eq!(body["code"], 1003);
    // 角色不是 customer_finance 时服务层拒绝（读者角色审核 → 403）。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/reject"),
        Some(r#"{"reason":"测试"}"#),
        Some(&tr),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "非财务角色应 403");

    // 2. 列表形状：invoice_request_id 数字、user_id 与审核人 ID 为字符串、queue 回退。
    let (_, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/invoices",
        None,
        Some(&t1),
    )
    .await;
    let items = body["data"]["items"].as_array().expect("items 应为数组");
    let row = items
        .iter()
        .find(|r| r["invoice_request_id"].as_i64() == Some(invoice_a as i64))
        .expect("种子发票应在列表");
    assert_eq!(row["user_id"], user_a.to_string(), "user_id 应为字符串");
    assert_eq!(row["review_status"], "pending");
    assert_eq!(
        row["queue_review_status"], "pending",
        "无审核记录时回退申请单"
    );
    assert_eq!(row["total_cents"], 400);
    assert_eq!(row["biz_type"], "charge");
    assert!(row["invoice_no"].as_str().unwrap().starts_with("INV-"));
    assert!(row["created_at"].as_str().is_some());

    // 3. 审核校验：approve 缺 HTTPS 链接 400；reject 空原因 400；未知字段 400。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"http://example.test/a.pdf"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "HTTP 链接应 400: {body}");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/reject"),
        Some(r#"{"reason":"  "}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空白原因应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/a.pdf","extra":1}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "未知字段应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        "/api/v1/admin/billing/invoices/0/approve",
        Some(r#"{"invoice_url":"https://example.test/a.pdf"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "ID=0 应 400");

    // 4. 双审：首审 awaiting_second → 同人复核 409 → 链接不一致 409 → 一致 approved。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/a.pdf"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "首审应成功: {body}");
    assert_eq!(body["data"]["review_status"], "awaiting_second");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/a.pdf"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "同人复核应 409");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/other.pdf"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "链接不一致应 409");
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_a}/approve"),
        Some(r#"{"invoice_url":"https://example.test/a.pdf"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "第二人复核应成功: {body}");
    assert_eq!(body["data"]["review_status"], "approved");
    let issued: String = sqlx::query_scalar(
        "SELECT CAST(`review_status` AS CHAR) FROM `invoice_request` WHERE `id` = ?",
    )
    .bind(invoice_a as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(issued, "issued", "双审通过后申请单应 issued");
    // 列表回填 queue_review_status=approved 与审核人字符串 ID。
    let (_, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/invoices",
        None,
        Some(&t1),
    )
    .await;
    let items = body["data"]["items"].as_array().unwrap();
    let row = items
        .iter()
        .find(|r| r["invoice_request_id"].as_i64() == Some(invoice_a as i64))
        .unwrap();
    assert_eq!(row["queue_review_status"], "approved");
    assert_eq!(
        row["first_reviewer_id"],
        fin1.admin_id.to_string(),
        "审核人 ID 应为字符串"
    );
    assert_eq!(row["second_reviewer_id"], fin2.admin_id.to_string());
    assert_eq!(row["invoice_url"], "https://example.test/a.pdf");

    // 5. 驳回：单人即可，申请单变 rejected。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_b}/reject"),
        Some(r#"{"reason":"抬头信息有误"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "驳回应成功: {body}");
    assert_eq!(body["data"]["review_status"], "rejected");
    // 已终态再操作 409。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/invoices/{invoice_b}/approve"),
        Some(r#"{"invoice_url":"https://example.test/b.pdf"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "已驳回再审批应 409");
    // 不存在的发票 404。
    let (status, _) = req_str(
        &router,
        "POST",
        "/api/v1/admin/billing/invoices/999999999/approve",
        Some(r#"{"invoice_url":"https://example.test/x.pdf"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "不存在的发票应 404");

    // 6. 审计：invoice_a 两条（awaiting_second + approved），invoice_b 一条（rejected）。
    let audited_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `target_type` = 'invoice' \
         AND `target_id` = ? AND `action` IN ('awaiting_second','approved')",
    )
    .bind(invoice_a.to_string())
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audited_a, 2, "发票双审应留两条审计");
    let audited_b: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `target_type` = 'invoice' \
         AND `target_id` = ? AND `action` = 'rejected'",
    )
    .bind(invoice_b.to_string())
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audited_b, 1, "驳回应留一条审计");

    cleanup(&db, &[], &[invoice_a, invoice_b], &[fin1, fin2, reader]).await;
    for uid in [user_a, user_b] {
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(uid as i64)
            .execute(db.as_ref())
            .await;
    }
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn meter_review_list_propose_decide_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let suf = rand_suffix();
    let fin1 = seed_finance_admin(&db, &alloc, &format!("m1{suf}")).await;
    let fin2 = seed_finance_admin(&db, &alloc, &format!("m2{suf}")).await;
    let reader = seed_admin(&db, &alloc, &format!("mr{suf}"), &["finance.read"]).await;
    let t1 = login_admin(&router, &fin1.username, "finance-pass-123").await;
    let t2 = login_admin(&router, &fin2.username, "finance-pass-123").await;
    let tr = login_admin(&router, &reader.username, "finance-pass-123").await;

    // 种子：异常计量（分段合计 250Wh ≠ 总量 200Wh）→ 转人工。
    let (order_id, payment_id, user_id, rule_id, station_id, device_id) =
        seed_completed_charge(&db, &alloc, 200).await;
    let now = chrono::Utc::now();
    let bad_meter = json!({
        "segments": [
            {"started_at": (now - chrono::Duration::minutes(10)).to_rfc3339(),
             "ended_at": (now - chrono::Duration::minutes(5)).to_rfc3339(), "energy_wh": 100, "peak_w": 1500},
            {"started_at": (now - chrono::Duration::minutes(5)).to_rfc3339(),
             "ended_at": now.to_rfc3339(), "energy_wh": 150, "peak_w": 1500},
        ],
        "charged_wh": 200, "charged_seconds": 600,
        "ended_at": now.to_rfc3339(), "stop_reason": 0
    });
    sqlx::query("UPDATE `charge_end_receipt` SET `meter_json` = CAST(? AS JSON) WHERE `charge_order_id` = ?")
        .bind(bad_meter.to_string()).bind(order_id as i64)
        .execute(db.as_ref()).await.unwrap();
    let _ = chargepilot_mod_settlement::billing::process_job(&db, order_id).await;
    let job: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `charge_billing_job` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(job, "manual_review", "分段守恒争议应转人工");
    // 计费转人工应同事务写入核实工单（旧版 EnqueueManualFeeReview，uk_order 幂等）。
    let (review_reason, review_wh): (String, u64) = sqlx::query_as(
        "SELECT `reason`, `energy_wh` FROM `manual_fee_review` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .expect("转人工应自动入队核实工单");
    assert_eq!(review_reason, "实际计量或计费快照需核实");
    assert_eq!(review_wh, 200, "工单应带已确认电量");

    // 1. 列表：pending 状态命中（关键词搜订单号），source 内 user_id 为字符串，reviews 初始为空。
    let order_no: String =
        sqlx::query_scalar("SELECT CAST(`order_no` AS CHAR) FROM `charge_order` WHERE `id` = ?")
            .bind(order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    // 工单行由计费转人工自动写入（见上方断言），不再手工种行。

    let (status, body) = req_str(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/billing/meter-reviews?page=1&page_size=20&status=pending&keyword={order_no}"
        ),
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "列表应成功: {body}");
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["page_size"], 20);
    // 模块级直调对照（定位 HTTP 层与 SQL 层差异）。
    let (direct, dtotal) = chargepilot_mod_settlement::meter_review::list_manual_fee_reviews(
        &db, "pending", &order_no, 1, 20,
    )
    .await
    .unwrap();
    eprintln!("DEBUG direct total={dtotal} rows={}", direct.len());
    let (direct_all, atotal) =
        chargepilot_mod_settlement::meter_review::list_manual_fee_reviews(&db, "", "", 1, 20)
            .await
            .unwrap();
    eprintln!("DEBUG direct-all total={atotal} rows={}", direct_all.len());
    let items = body["data"]["items"].as_array().expect("items 应为数组");
    let row = items
        .iter()
        .find(|r| r["charge_order_id"].as_u64() == Some(order_id))
        .unwrap_or_else(|| panic!("转人工订单 {order_id} 应在队列: {}", body));
    assert_eq!(row["status"], "pending");
    assert_eq!(
        row["source"]["user_id"],
        user_id.to_string(),
        "source.user_id 应为字符串"
    );
    assert!(row["source"]["meter"]["charged_wh"].as_u64().is_some());
    assert_eq!(
        row["reviews"].as_array().map(Vec::len),
        Some(0),
        "初始无核实记录"
    );
    // 非法状态白名单 400；无权限写操作 403。
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/meter-reviews?status=resolved",
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolved 白名单应可用");
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/meter-reviews?status=bogus",
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法状态应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/propose"),
        Some(
            r#"{"request_id":"11111111-1111-4111-8111-111111111111","reason":"x","segments":[{"started_at":"2026-01-01T00:00:00Z","ended_at":"2026-01-01T00:05:00Z","energy_wh":100}]}"#,
        ),
        Some(&tr),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "无 billing.meter.review 应 403"
    );

    // 2. propose 校验：非 UUID / 空依据 / 空分段 400。
    let segments_json = json!([
        {"started_at": (now - chrono::Duration::minutes(10)).to_rfc3339(),
         "ended_at": (now - chrono::Duration::minutes(5)).to_rfc3339(), "energy_wh": 100, "peak_w": 1500},
        {"started_at": (now - chrono::Duration::minutes(5)).to_rfc3339(),
         "ended_at": now.to_rfc3339(), "energy_wh": 100, "peak_w": 1500},
    ]);
    let propose_body = |req_id: &str, reason: &str| {
        json!({"request_id": req_id, "reason": reason, "segments": segments_json}).to_string()
    };
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/propose"),
        Some(&propose_body("not-a-uuid", "分段修正")),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非 UUID 应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/propose"),
        Some(&propose_body("11111111-1111-4111-8111-111111111111", "  ")),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空依据应 400");

    // 3. propose 成功：返回完整核实行（status=awaiting_second、corrected 内嵌对象）。
    // request_id 每次运行随机（同号残留会触发旧版重放核对冲突）。
    let request_id = format!(
        "11111111-1111-4111-8111-{:012}",
        u32::from_str_radix(&rand_suffix(), 16).unwrap() as u64
    );
    let request_id = request_id.as_str();
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/propose"),
        Some(&propose_body(
            request_id,
            "分段合计与总量不一致，修正分段读数",
        )),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "提案应成功: {body}");
    let review_id = body["data"]["id"]
        .as_u64()
        .unwrap_or_else(|| panic!("应返回核实记录 ID: {body}"));
    assert_eq!(body["data"]["charge_order_id"], order_id);
    assert_eq!(body["data"]["status"], "awaiting_second");
    assert_eq!(body["data"]["first_reviewer_id"], fin1.admin_id);
    assert!(
        body["data"]["corrected"]["meter"]["segments"]
            .as_array()
            .is_some()
    );
    // 同请求重放复用首次记录（幂等静默）。
    let (status, replay) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/propose"),
        Some(&propose_body(
            request_id,
            "分段合计与总量不一致，修正分段读数",
        )),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["data"]["id"], review_id, "同请求重放应复用首次记录");

    // 4. decide 校验：缺 approve 400；驳回缺依据 400。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/decide"),
        Some(&json!({"review_id": review_id, "reason": "x"}).to_string()),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "缺 approve 应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/decide"),
        Some(&json!({"review_id": review_id, "approve": false, "reason": ""}).to_string()),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "驳回缺依据应 400");
    // 首审人自行复核冲突。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/decide"),
        Some(&json!({"review_id": review_id, "approve": true, "reason": ""}).to_string()),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "首审人自行复核应 409");

    // 5. 第二人批准 → 200 {"approved":true}；任务重排队后重算入账。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/decide"),
        Some(&json!({"review_id": review_id, "approve": true, "reason": ""}).to_string()),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "复核批准应成功: {body}");
    assert_eq!(body["data"]["approved"], true);
    let job: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `charge_billing_job` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(job, "pending", "批准后任务应重排队");
    let handled = chargepilot_mod_settlement::billing::process_job(&db, order_id)
        .await
        .expect("重算应成功");
    assert!(handled, "修正分段应可入账");
    let total: i64 = sqlx::query_scalar("SELECT `total_cents` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert_eq!(total, 16, "修正 200Wh 应入账 16 分");
    // 入账事务应关闭待核实工单（旧版 fee_calculation_repo 的 pending→resolved）。
    let ticket: (String, Option<String>) = sqlx::query_as(
        "SELECT CAST(`status` AS CHAR), CAST(`resolved_at` AS CHAR) \
         FROM `manual_fee_review` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(ticket.0, "resolved", "入账后工单应 resolved");
    assert!(ticket.1.is_some(), "resolved_at 应有值");
    // 重复同一决定重放静默成功（旧版幂等分支）。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/meter-reviews/{order_id}/decide"),
        Some(&json!({"review_id": review_id, "approve": true, "reason": ""}).to_string()),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "同决定重放应静默成功");

    // 6. 列表回填 reviews；审计断言。工单由计费转人工自动入队、入账事务关闭
    // （pending→resolved），此处按 resolved 列表验证回填。
    let (_, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/billing/meter-reviews?status=resolved&keyword={order_no}"),
        None,
        Some(&t2),
    )
    .await;
    let items = body["data"]["items"].as_array().unwrap();
    let row = items
        .iter()
        .find(|r| r["charge_order_id"].as_u64() == Some(order_id));
    assert!(
        row.is_some(),
        "入账后订单应在 resolved 列表且回填核实记录: {}",
        serde_json::to_string(&body).unwrap_or_default()
    );
    let reviews = row.unwrap()["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1, "应回填一条核实记录");
    assert_eq!(reviews[0]["status"], "approved");
    assert_eq!(reviews[0]["second_reviewer_id"], fin2.admin_id);
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `target_type` = 'meter_review' \
         AND `target_id` = ? AND `action` IN ('propose','decide')",
    )
    .bind(review_id.to_string())
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audited, 2, "提案与决定各留一条审计");

    cleanup(
        &db,
        &[(
            order_id, payment_id, user_id, rule_id, station_id, device_id,
        )],
        &[],
        &[fin1, fin2, reader],
    )
    .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn refund_list_create_review_retry_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let suf = rand_suffix();
    let fin1 = seed_finance_admin(&db, &alloc, &format!("f1{suf}")).await;
    let fin2 = seed_finance_admin(&db, &alloc, &format!("f2{suf}")).await;
    let reader = seed_admin(
        &db,
        &alloc,
        &format!("fr{suf}"),
        &["finance.read", "finance.refund.read"],
    )
    .await;
    let t1 = login_admin(&router, &fin1.username, "finance-pass-123").await;
    let t2 = login_admin(&router, &fin2.username, "finance-pass-123").await;
    let tr = login_admin(&router, &reader.username, "finance-pass-123").await;

    // 种子：完成计费的订单。实耗 2000Wh 理论 160 分超出预付 100 分 → 封顶 100 分且
    // 无余款退款，订单保持 completed（人工退款要求订单未处于退款中）。
    let (order_id, payment_id, user_id, rule_id, station_id, device_id) =
        seed_completed_charge(&db, &alloc, 2000).await;
    let handled = chargepilot_mod_settlement::billing::process_job(&db, order_id)
        .await
        .expect("计费应成功");
    assert!(handled, "2000Wh 应封顶入账 100 分且无余款退款");
    let auto_refund: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `refund_record` WHERE `payment_order_id` = ? AND `refund_no` LIKE 'FEE-R%'",
    )
    .bind(payment_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        auto_refund, 0,
        "实费封顶后无余款自动退款，订单保持 completed 可人工退款"
    );

    // 1. 列表权限与参数校验。
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/refunds",
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "列表应成功");
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/refunds?status=bogus",
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法状态应 400");
    let long_no = "R".repeat(65);
    let (status, _) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/billing/refunds?refund_no={long_no}"),
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "退款单号过长应 400");

    // 2. 申请校验与幂等：非法 UUID / 非正金额 400；无 order.refund.create 403。
    let request_id = format!(
        "22222222-2222-4222-8222-{:012}",
        u32::from_str_radix(&rand_suffix(), 16).unwrap() as u64
    );
    let request_id = request_id.as_str();
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(&json!({"request_id": "bad", "amount_cents": 50, "reason": "协商退款"}).to_string()),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法 UUID 应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(
            &json!({"request_id": request_id, "amount_cents": 0, "reason": "协商退款"}).to_string(),
        ),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "零金额应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(
            &json!({"request_id": request_id, "amount_cents": 50, "reason": "协商退款"})
                .to_string(),
        ),
        Some(&tr),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无申请权限应 403");

    // 3. 申请成功 → created=true + MR 单号；重放 created=false 同单号。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(
            &json!({"request_id": request_id, "amount_cents": 50, "reason": "协商退款"})
                .to_string(),
        ),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "申请应成功: {body}");
    assert_eq!(body["data"]["created"], true);
    assert_eq!(body["data"]["request_id"], request_id);
    let refund_no = body["data"]["refund_no"]
        .as_str()
        .expect("应返回退款单号")
        .to_string();
    assert!(refund_no.starts_with("MR"));
    let (status, replay) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(
            &json!({"request_id": request_id, "amount_cents": 50, "reason": "协商退款"})
                .to_string(),
        ),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["data"]["created"], false, "重放不得重复建单");
    assert_eq!(replay["data"]["refund_no"], refund_no);

    // 4. 列表形状：refund_no 精确过滤命中；扁平字段与旧版 GORM 投影一致。
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/billing/refunds?page=1&page_size=20&refund_no={refund_no}"),
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "单号过滤应成功: {body}");
    assert_eq!(body["data"]["total"], 1);
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row["refund_no"], refund_no);
    assert_eq!(row["user_id"], user_id, "user_id 保持数字（旧版 map 投影）");
    assert_eq!(row["refund_cents"], 50);
    assert_eq!(row["status"], "pending");
    assert_eq!(row["biz_type"], "charge");
    assert_eq!(row["execution_policy"], "manual_review");
    assert_eq!(row["reason"], "协商退款");
    assert!(row["created_at"].as_str().is_some());
    assert!(row["next_attempt_at"].as_str().is_some());
    assert!(row["snapshot_json"].is_null(), "未审核时审核列为 null");
    assert!(row["first_signer"].is_null());

    // 5. 双审：approve_comment/reason 校验；首审 awaiting_second → 列表回填 first_signer →
    //    二审 approved → execution_policy=automatic。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no}/approve"),
        Some(r#"{"approve_comment":"  "}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空白意见应 400");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no}/reject"),
        Some(r#"{"reason":"凭证不足，不予退款"}"#),
        Some(&t1),
    )
    .await;
    // 驳回单人即可：第一单验证驳回路径，第二单走双审通过主流程。
    assert_eq!(status, StatusCode::OK, "驳回应成功");
    // 上面已把该单驳回，重新用新 request_id 申请第二单做双审主流程。
    let request_id2 = format!(
        "33333333-3333-4333-8333-{:012}",
        u32::from_str_radix(&rand_suffix(), 16).unwrap() as u64
    );
    let request_id2 = request_id2.as_str();
    let (status, body2) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/orders/{order_id}/refunds"),
        Some(
            &json!({"request_id": request_id2, "amount_cents": 20, "reason": "第二单协商退款"})
                .to_string(),
        ),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "第二单申请应成功: {body2}");
    let refund_no2 = body2["data"]["refund_no"].as_str().unwrap().to_string();
    // 驳回单终态确认。
    let rejected_status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `refund_record` WHERE CAST(`refund_no` AS CHAR) = ?",
    )
    .bind(&refund_no)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(rejected_status, "rejected", "首单应已驳回");
    // 列表按状态筛选 rejected 命中首单。
    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/billing/refunds?status=rejected",
        None,
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hit = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["refund_no"] == refund_no);
    assert!(hit, "rejected 筛选应命中首单");
    // 首单已终态后再审 → 409。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no}/approve"),
        Some(r#"{"approve_comment":"复核"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "已驳回单再审应 409");

    // 第二单双审通过主流程。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/approve"),
        Some(r#"{"approve_comment":"核实金额无误"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "首审应成功: {body}");
    assert_eq!(body["data"]["review_status"], "awaiting_second");
    // 同人二审冲突。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/approve"),
        Some(r#"{"approve_comment":"再核"}"#),
        Some(&t2),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "同人二审应 409");
    // 列表回填 first_signer。
    let (_, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/billing/refunds?refund_no={refund_no2}"),
        None,
        Some(&t1),
    )
    .await;
    let row = &body["data"]["items"].as_array().unwrap()[0];
    assert_eq!(row["first_signer"], fin2.admin_id);
    assert_eq!(row["first_comment"], "核实金额无误");
    assert!(
        row["snapshot_json"].as_str().is_some(),
        "首审快照应为 JSON 文本"
    );
    // 不同人二审通过。
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/approve"),
        Some(r#"{"approve_comment":"复核同意"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "二审应成功: {body}");
    assert_eq!(body["data"]["review_status"], "approved");
    let (policy, status_now): (String, String) = sqlx::query_as(
        "SELECT CAST(`execution_policy` AS CHAR), CAST(`status` AS CHAR) \
         FROM `refund_record` WHERE CAST(`refund_no` AS CHAR) = ?",
    )
    .bind(&refund_no2)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (policy.as_str(), status_now.as_str()),
        ("automatic", "pending")
    );

    // 6. retry：automatic+pending 可重试 → {"scheduled":true}；无权限 403；重试后仍 automatic。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/retry"),
        Some(r#"{"reason":"渠道超时重试"}"#),
        Some(&tr),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无 retry 权限应 403");
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/retry"),
        Some(r#"{"reason":"  "}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空白重试原因应 400");
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no2}/retry"),
        Some(r#"{"reason":"渠道超时重试"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重试应成功: {body}");
    assert_eq!(body["data"]["scheduled"], true);
    // 驳回终态单 retry → 409。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/billing/refunds/{refund_no}/retry"),
        Some(r#"{"reason":"尝试重试已驳回单"}"#),
        Some(&t1),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "已驳回单重试应 409");

    // 7. 审计：申请/首审/二审/重试各留痕。
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `target_type` = 'refund' \
         AND `action` IN ('request','awaiting_second','approved','retry')",
    )
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert!(
        audited >= 5,
        "退款链路审计应齐全（request×2 + rejected + awaiting_second + approved + retry）: {audited}"
    );

    cleanup(
        &db,
        &[(
            order_id, payment_id, user_id, rule_id, station_id, device_id,
        )],
        &[],
        &[fin1, fin2, reader],
    )
    .await;
}
