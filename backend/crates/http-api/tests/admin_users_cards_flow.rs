#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 后台用户与在线卡主缝集成测试：用户列表/详情/充值页、卡列表/绑定/状态变更
//! 与权限分支（旧版 admin/user 契约对齐）。种子自建自清理，全局流锁串行。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::json;
use support::{TestApp, flow_lock, login_admin_opt, rand_suffix, req_str};

/// SQL 直建受限管理员（角色 permissions = 指定权限码）。
async fn seed_limited_admin(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    suffix: &str,
    permissions: &[&str],
) -> (String, u64, u64) {
    let username = format!("uc-lim-{suffix}");
    let (role_id, admin_id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '用户卡受限测试角色', 0)",
    )
    .bind(role_id as i64)
    .bind(format!("uc_lim_{suffix}"))
    .execute(db)
    .await
    .unwrap();
    for p in permissions {
        sqlx::query("INSERT INTO `role_permission` (`role_id`, `permission_id`) SELECT ?, `id` FROM `permission` WHERE `code` = ?")
            .bind(role_id as i64).bind(p).execute(db).await.unwrap();
    }
    let hash = chargepilot_platform::password::hash("uc-limited-pass").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)")
        .bind(admin_id as i64).bind(&username).bind(hash).bind(role_id as i64).execute(db).await.unwrap();
    (username, role_id, admin_id)
}

/// 本批种子（尾部自清理按这些 ID）。
struct Seed {
    user_a: u64,
    user_b: u64,
    phone_a: String,
    nickname_a: String,
    payment_recharge: u64,
    payment_charge: u64,
    order_1: u64,
    order_2: u64,
    station_id: u64,
    card_active_id: u64,
    card_active_no: String,
    card_unbound_id: u64,
    card_unbound_no: String,
    super_id: u64,
    lim_role_id: u64,
    lim_admin_id: u64,
}

async fn seed(db: &sqlx::MySqlPool, alloc: &SegmentAllocator) -> Seed {
    let suf = rand_suffix();
    // 自愈：清理历史失败运行残留的测试意图（端口唯一键占用）。
    let _ = sqlx::query("DELETE FROM `charge_payment_intent` WHERE `port_code` LIKE 'ucp-%'")
        .execute(db)
        .await;
    let user_a = alloc.next_id().await.unwrap();
    let user_b = alloc.next_id().await.unwrap();
    let wallet_id = alloc.next_id().await.unwrap();
    let payment_recharge = alloc.next_id().await.unwrap();
    let payment_charge = alloc.next_id().await.unwrap();
    let order_1 = alloc.next_id().await.unwrap();
    let order_2 = alloc.next_id().await.unwrap();
    let station_id = alloc.next_id().await.unwrap();
    let card_active_id = alloc.next_id().await.unwrap();
    let card_unbound_id = alloc.next_id().await.unwrap();
    let super_id = alloc.next_id().await.unwrap();
    let phone_a = format!("139{:08}", alloc.next_id().await.unwrap() % 100_000_000);
    let nickname_a = format!("uc-nick-{suf}");
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());

    // 充电用户 A（active，含手机号/昵称）与 B（frozen）。
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `phone`, `gender`, `status`) VALUES (?, CAST(? AS BINARY), ?, ?, 'unknown', 'active')")
        .bind(user_a as i64).bind(format!("uc-a-{user_a}")).bind(&nickname_a).bind(&phone_a)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '卡测冻结用户', 'frozen')")
        .bind(user_b as i64).bind(format!("uc-b-{user_b}")).execute(db).await.unwrap();
    // A 的钱包：可用 700 冻结 100。
    sqlx::query("INSERT INTO `wallet_account` (`id`, `user_id`, `balance_cents`, `frozen_cents`, `version`, `status`) VALUES (?, ?, 700, 100, 1, 'active')")
        .bind(wallet_id as i64).bind(user_a as i64).execute(db).await.unwrap();

    // A 的充值支付单（paid 500）与充电支付单（不进充值页）。
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'wallet_recharge', ?, ?, 'wechat', 500, 500, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_recharge as i64).bind(format!("P{payment_recharge}"))
        .bind(payment_recharge as i64).bind(user_a as i64).bind(month)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) VALUES (?, ?, 'charge', ?, ?, 'wechat', 70, 70, 'paid', UTC_TIMESTAMP(3), ?)")
        .bind(payment_charge as i64).bind(format!("P{payment_charge}"))
        .bind(order_2 as i64).bind(user_a as i64).bind(month)
        .execute(db).await.unwrap();

    // A 的两笔订单：order_1 早且无意图，order_2 晚且带支付意图（站点补充）。
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `total_cents`, `created_at`, `created_month`) VALUES (?, ?, ?, 'uc-dev-1', 1, 'uc-p1', ?, 'completed', 'paid', 30, DATE_SUB(NOW(3), INTERVAL 2 HOUR), ?)")
        .bind(order_1 as i64).bind(format!("C{order_1}")).bind(user_a as i64)
        .bind(payment_charge as i64).bind(month).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, `status`, `payment_status`, `total_cents`, `created_at`, `created_month`) VALUES (?, ?, ?, 'uc-dev-2', 1, 'uc-p2', ?, 'completed', 'paid', 70, NOW(3), ?)")
        .bind(order_2 as i64).bind(format!("C{order_2}")).bind(user_a as i64)
        .bind(payment_charge as i64).bind(month).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `merchant_order_no`, `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, `port_code`, `station_id`, `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, `charge_quantity`, `status`, `expires_at`, `charge_order_id`) VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), 'uc-dev-2', 1, ?, ?, 1, 1, CAST('{}' AS JSON), 1.000, 60, 50, 20, 70, 1, 60, 'paid', DATE_ADD(NOW(3), INTERVAL 30 MINUTE), ?)")
        .bind(format!("uc-intent-{order_2}")).bind(format!("uc-req-{order_2}"))
        .bind(format!("M{payment_charge}")).bind(payment_charge as i64)
        .bind(user_a as i64).bind(format!("uc-a-{user_a}"))
        .bind(format!("ucp-{suf}"))
        .bind(station_id as i64).bind(order_2 as i64)
        .execute(db).await.unwrap();

    // 券 2 张（1 未使用 1 已使用）+ 报障 1 单。
    sqlx::query("INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, `expired_at`) VALUES (?, ?, 'manual', 'unused', DATE_ADD(NOW(3), INTERVAL 30 DAY))")
        .bind(order_1 as i64).bind(user_a as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, `used_at`, `expired_at`) VALUES (?, ?, 'manual', 'used', NOW(3), DATE_ADD(NOW(3), INTERVAL 30 DAY))")
        .bind(order_2 as i64).bind(user_a as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_fault_report` (`device_id`, `user_id`, `fault_type`) VALUES ('uc-dev-1', ?, 'other')")
        .bind(user_a as i64).execute(db).await.unwrap();

    // 在线卡：A 的 active 卡与 B 的 unbound 卡（解绑不可逆回归用）。
    let card_active_no = format!("77{card_active_id}");
    let card_unbound_no = format!("88{card_unbound_id}");
    sqlx::query("INSERT INTO `online_card` (`id`, `card_no`, `user_id`, `status`) VALUES (?, ?, ?, 'active')")
        .bind(card_active_id as i64).bind(&card_active_no).bind(user_a as i64)
        .execute(db).await.unwrap();
    sqlx::query("INSERT INTO `online_card` (`id`, `card_no`, `user_id`, `status`) VALUES (?, ?, ?, 'unbound')")
        .bind(card_unbound_id as i64).bind(&card_unbound_no).bind(user_b as i64)
        .execute(db).await.unwrap();

    // 超管（customer_admin 内置全权限）。
    let hash = chargepilot_platform::password::hash("uc-super-pass").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(super_id as i64).bind(format!("uc-root-{suf}")).bind(hash)
        .execute(db).await.unwrap();
    let (lim_name, lim_role_id, lim_admin_id) =
        seed_limited_admin(db, alloc, &suf, &["user.read"]).await;
    let _ = lim_name;

    Seed {
        user_a,
        user_b,
        phone_a,
        nickname_a,
        payment_recharge,
        payment_charge,
        order_1,
        order_2,
        station_id,
        card_active_id,
        card_active_no,
        card_unbound_id,
        card_unbound_no,
        super_id,
        lim_role_id,
        lim_admin_id,
    }
}

/// 尾部自清理：按主键与前缀删除本批种子（审计流水保留按归属清理）。
async fn cleanup(db: &sqlx::MySqlPool, s: &Seed) {
    let _ = sqlx::query("DELETE FROM `online_card_audit` WHERE `card_id` IN (SELECT `id` FROM `online_card` WHERE `card_no` LIKE '77%' OR `card_no` LIKE '88%') OR `actor_id` = ?")
        .bind(s.super_id as i64).execute(db).await;
    let _ =
        sqlx::query("DELETE FROM `online_card` WHERE `card_no` LIKE '77%' OR `card_no` LIKE '88%'")
            .execute(db)
            .await;
    let _ = sqlx::query("DELETE FROM `device_fault_report` WHERE `user_id` = ?")
        .bind(s.user_a as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `coupon_grant` WHERE `user_id` = ?")
        .bind(s.user_a as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `charge_payment_intent` WHERE `charge_order_id` IN (?, ?)")
        .bind(s.order_1 as i64)
        .bind(s.order_2 as i64)
        .execute(db)
        .await;
    for id in [s.order_1, s.order_2] {
        let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
            .bind(id as i64)
            .execute(db)
            .await;
    }
    for id in [s.payment_recharge, s.payment_charge] {
        let _ = sqlx::query("DELETE FROM `payment_order` WHERE `id` = ?")
            .bind(id as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query("DELETE FROM `wallet_account` WHERE `user_id` IN (?, ?)")
        .bind(s.user_a as i64)
        .bind(s.user_b as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `id` IN (?, ?)")
        .bind(s.user_a as i64)
        .bind(s.user_b as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
        .bind(s.lim_role_id as i64)
        .execute(db)
        .await;
    for id in [s.lim_admin_id, s.super_id] {
        let _ = sqlx::query("DELETE FROM `admin_user` WHERE `id` = ?")
            .bind(id as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query("DELETE FROM `role` WHERE `id` = ?")
        .bind(s.lim_role_id as i64)
        .execute(db)
        .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn admin_users_and_online_cards_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().with_wallet().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let seed = seed(db.as_ref(), alloc.as_ref()).await;
    let super_name: String =
        sqlx::query_scalar("SELECT `username` FROM `admin_user` WHERE `id` = ?")
            .bind(seed.super_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    let super_token = login_admin_opt(&router, &super_name, "uc-super-pass")
        .await
        .expect("超管登录");
    let lim_name: String = sqlx::query_scalar("SELECT `username` FROM `admin_user` WHERE `id` = ?")
        .bind(seed.lim_admin_id as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    let lim_token = login_admin_opt(&router, &lim_name, "uc-limited-pass")
        .await
        .expect("受限登录");

    // ---- 1. 权限分支：受限账号（user.read）可读用户与卡列表，不能写卡。 ----
    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/users?page=1&page_size=5",
        None,
        Some(&lim_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["items"].is_array());
    assert!(
        body["data"]["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "user.read")
    );
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/online-cards",
        None,
        Some(&lim_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "卡列表仅要求 user.read");
    let (status, body) = req_str(
        &router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(r#"{"card_no":"123","user_id":"1","reason":"无权限绑定"}"#),
        Some(&lim_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "绑定应被拒绝: {body}");
    assert_eq!(body["code"], 1003);
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_active_id),
        Some(r#"{"status":"disabled","reason":"无权限停用"}"#),
        Some(&lim_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "状态变更应被拒绝");

    // ---- 2. 用户列表：形状、ID 字符串契约、手机号/关键词/状态筛选与参数校验。 ----
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users?keyword={}", seed.phone_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "完整手机号精确命中 A: {body}");
    let row = &items[0];
    assert_eq!(row["id"], seed.user_a.to_string(), "id 按十进制字符串输出");
    assert_eq!(row["phone"], seed.phone_a.as_str());
    assert_eq!(row["phone_bound"], true);
    assert_eq!(row["balance_cents"], 700);
    assert_eq!(row["frozen_cents"], 100);
    assert_eq!(row["order_count"], 2);
    assert_eq!(row["total_cents"], 100, "30 + 70 累计消费");
    assert!(row["last_order_at"].is_string());
    assert_eq!(row["status"], "active");
    assert!(row["inviter_id"].is_null());
    assert_eq!(row["gender"], "unknown");

    let (status, body) = req_str(
        &router,
        "GET",
        &format!(
            "/api/v1/admin/users?keyword={}&page_size=100",
            &seed.phone_a[..5]
        ),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "号码片段模糊命中: {body}");
    let want_a = seed.user_a.to_string();
    assert!(
        body["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"].as_str() == Some(want_a.as_str())),
        "号码片段应命中 A"
    );
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users?keyword={}", seed.nickname_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "昵称关键词应命中: {body}");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);

    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/users?status=frozen&page_size=100",
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frozen_items = body["data"]["items"].as_array().unwrap();
    let (want_a, want_b) = (seed.user_a.to_string(), seed.user_b.to_string());
    assert!(
        frozen_items
            .iter()
            .any(|u| u["id"].as_str() == Some(want_b.as_str())),
        "frozen 筛选应含 B"
    );
    assert!(
        !frozen_items
            .iter()
            .any(|u| u["id"].as_str() == Some(want_a.as_str())),
        "frozen 筛选不应含 A"
    );

    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/users?status=bogus",
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法状态应 400: {body}");
    assert_eq!(body["code"], 1005);
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/users?page=0",
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "页码越界应 400");

    // ---- 3. 用户详情：钱包状态、统计计数与最近订单。 ----
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}", seed.user_a),
        None,
        Some(&lim_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "详情仅需 user.read: {body}");
    let detail = &body["data"];
    assert_eq!(detail["wallet_status"], "active");
    assert_eq!(detail["coupon_granted"], 2);
    assert_eq!(detail["coupon_unused"], 1);
    assert_eq!(detail["fault_reports"], 1);
    let orders = detail["recent_orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2, "最近订单两条");
    assert_eq!(orders[0]["order_id"], seed.order_2, "按订单 ID 倒序");
    assert_eq!(
        orders[0]["station_id"], seed.station_id,
        "经支付意图补充站点"
    );
    assert_eq!(orders[0]["business_status"], "completed");
    assert_eq!(orders[0]["payment_status"], "paid");
    assert_eq!(orders[0]["total_cents"], 70);
    assert!(orders[1]["station_id"].is_null(), "无意图订单站点为 null");

    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}", seed.user_a + 5_000_000),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "缺失用户应 404: {body}");
    assert_eq!(body["code"], 1004);
    let (status, _) = req_str(
        &router,
        "GET",
        "/api/v1/admin/users/not-a-number",
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法 ID 应 400");

    // ---- 4. 充值记录：仅 wallet_recharge、字符串用户 ID 与参数校验。 ----
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges", seed.user_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["total"], 1);
    let recharges = body["data"]["items"].as_array().unwrap();
    assert_eq!(recharges.len(), 1);
    assert_eq!(recharges[0]["payment_order_id"], seed.payment_recharge);
    assert_eq!(recharges[0]["biz_type"], "wallet_recharge");
    assert_eq!(recharges[0]["user_id"], seed.user_a.to_string());
    assert_eq!(recharges[0]["payment_status"], "paid");
    assert_eq!(recharges[0]["total_cents"], 500);
    assert_eq!(recharges[0]["paid_cents"], 500);
    assert!(recharges[0]["paid_at"].is_string());
    assert!(recharges[0]["charge_order_id"].is_null());

    let (status, _) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges", seed.user_b),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges?keyword=x", seed.user_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "额外参数应拒绝: {body}");
    let (status, _) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges?page=abc", seed.user_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges?page=", seed.user_a),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空值分页参数应拒绝");
    let (status, _) = req_str(
        &router,
        "GET",
        &format!("/api/v1/admin/users/{}/recharges", seed.user_a + 5_000_000),
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "缺失用户充值页应 404");

    // ---- 5. 卡列表：{items, permissions} 形状。 ----
    let (status, body) = req_str(
        &router,
        "GET",
        "/api/v1/admin/online-cards",
        None,
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cards = body["data"]["items"].as_array().unwrap();
    assert!(body["data"]["permissions"].is_array());
    let active_card = cards
        .iter()
        .find(|c| c["id"] == seed.card_active_id)
        .expect("种子卡应在列表中");
    assert_eq!(active_card["card_no"], seed.card_active_no.as_str());
    assert_eq!(
        active_card["user_id"],
        seed.user_a.to_string(),
        "user_id 按字符串输出"
    );
    assert_eq!(active_card["status"], "active");
    let unbound_card = cards
        .iter()
        .find(|c| c["id"] == seed.card_unbound_id)
        .expect("unbound 卡应在列表中");
    assert_eq!(unbound_card["card_no"], seed.card_unbound_no.as_str());

    // ---- 6. 绑定事务：新卡、幂等重放、冲突、输入与目标用户校验。 ----
    let bind_no = format!("66{seed_user}", seed_user = seed.user_a % 1_000_000_000);
    let bind_body =
        json!({"card_no": bind_no, "user_id": seed.user_a.to_string(), "reason": "人工核验绑定"});
    let (status, body) = req_str(
        &router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(&bind_body.to_string()),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "绑定新卡应成功: {body}");
    let bound_id = body["data"]["id"].as_u64().expect("返回卡 ID");
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `online_card_audit` WHERE `card_id` = ? AND `action` = 'bind'",
    )
    .bind(bound_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audits, 1, "首次绑定应留一条审计");
    let (bound_user, bound_status): (u64, String) = sqlx::query_as(
        "SELECT `user_id`, CAST(`status` AS CHAR) FROM `online_card` WHERE `id` = ?",
    )
    .bind(bound_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!((bound_user, bound_status.as_str()), (seed.user_a, "active"));

    // 幂等重放：同用户已启用 → 200 同 ID 且不重复审计。
    let (status, body) = req_str(
        &router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(&bind_body.to_string()),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放应幂等成功: {body}");
    assert_eq!(body["data"]["id"], bound_id);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `online_card_audit` WHERE `card_id` = ? AND `action` = 'bind'",
    )
    .bind(bound_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audits, 1, "幂等重放不得重复审计");

    // 目标用户未启用：frozen 的 B 不能作为绑定目标 → 404（旧版先校验用户再锁卡）。
    let (status, _) = req_str(
        &router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(
            &json!({"card_no": "551000", "user_id": seed.user_b.to_string(), "reason": "冻结用户"})
                .to_string(),
        ),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "冻结目标用户应 404");

    // 冲突：把 A 已绑的卡绑给启用中的 B → 409；事务回滚不留新审计。
    sqlx::query("UPDATE `user` SET `status` = 'active' WHERE `id` = ?")
        .bind(seed.user_b as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let (status, body) = req_str(&router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(&json!({"card_no": seed.card_active_no, "user_id": seed.user_b.to_string(), "reason": "试图抢占"}).to_string()),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "绑定他人卡应冲突: {body}");
    assert!(
        body["message"].as_str().unwrap().contains("卡已绑定"),
        "冲突文案透出: {body}"
    );
    let audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `online_card_audit` WHERE `card_id` = ?")
            .bind(seed.card_active_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(audits, 0, "冲突回滚不得留审计");

    // 输入校验：前导零卡号、空依据、非法用户 ID、未启用用户。
    for bad in [
        json!({"card_no": "007", "user_id": seed.user_a.to_string(), "reason": "前导零"}),
        json!({"card_no": "123456", "user_id": seed.user_a.to_string(), "reason": "  "}),
        json!({"card_no": "123456", "user_id": "abc", "reason": "非法用户"}),
        json!({"card_no": "123456", "user_id": seed.user_a.to_string(), "reason": ""}),
    ] {
        let (status, _) = req_str(
            &router,
            "POST",
            "/api/v1/admin/online-cards/bind",
            Some(&bad.to_string()),
            Some(&super_token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "非法输入应 400: {bad}");
    }
    let (status, _) = req_str(&router,
        "POST",
        "/api/v1/admin/online-cards/bind",
        Some(&json!({"card_no": "123456", "user_id": (seed.user_a + 5_000_000).to_string(), "reason": "缺失用户"}).to_string()),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "绑到缺失用户应 404");

    // ---- 7. 状态变更事务：启停、解绑不可逆、输入与缺失校验。 ----
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_active_id),
        Some(r#"{"status":"disabled","reason":"运营停用"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["id"], seed.card_active_id);
    let card_status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `online_card` WHERE `id` = ?")
            .bind(seed.card_active_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(card_status, "disabled");
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `online_card_audit` WHERE `card_id` = ? AND `action` = 'disabled'",
    )
    .bind(seed.card_active_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(audits, 1, "状态变更应留审计");

    // 解绑后再启用 → 409。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_active_id),
        Some(r#"{"status":"unbound","reason":"用户解绑"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_active_id),
        Some(r#"{"status":"active","reason":"试图恢复"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "解绑卡不可恢复: {body}");
    assert!(body["message"].as_str().unwrap().contains("刷新后重试"));
    let card_status: String =
        sqlx::query_scalar("SELECT CAST(`status` AS CHAR) FROM `online_card` WHERE `id` = ?")
            .bind(seed.card_active_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(card_status, "unbound", "冲突后状态不变");

    // 同状态 unbound → unbound 允许（旧版约束只禁改回其他状态）。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_unbound_id),
        Some(r#"{"status":"unbound","reason":"保持解绑"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "同状态应允许: ");

    // 输入与缺失：未知状态 400、空依据 400、缺失卡 404。
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_unbound_id),
        Some(r#"{"status":"bogus","reason":"未知状态"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = req_str(
        &router,
        "POST",
        &format!("/api/v1/admin/online-cards/{}/status", seed.card_unbound_id),
        Some(r#"{"status":"active","reason":""}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = req_str(
        &router,
        "POST",
        &format!(
            "/api/v1/admin/online-cards/{}/status",
            seed.card_unbound_id + 5_000_000
        ),
        Some(r#"{"status":"active","reason":"缺失卡"}"#),
        Some(&super_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "缺失卡应 404");

    cleanup(db.as_ref(), &seed).await;
}
