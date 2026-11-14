//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 05 批 3 主缝集成测试：活动建规则校验 → 门槛触发发放（幂等）→
//! 单人/总量上限 → 邀请奖励（established 检查、双人发放、自邀拒绝）→
//! 管理列表/更新/审计。管理端走 HTTP，发放编排直调模块函数（旧版调用缝）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::ApiDeps;
use chargepilot_mod_marketing::activity::ActivityEvent;
use chargepilot_platform::ids::SegmentAllocator;
use chrono::Datelike as _;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .unwrap()
        })
        .clone()
}

/// 券域测试串行锁（共享 dev 库的 flow 测试互斥）。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn build() -> Option<(axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>)> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let Some(redis_url) = env_or_skip("TEST_REDIS_URL") else {
        panic!("集成测试需要 TEST_REDIS_URL（node tasks.mjs test-integration）");
    };
    let db = match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => d,
        Err(e) => {
            panic!("数据库连接失败：{e}");
        }
    };
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 60));
    if let Err(e) = allocator.warm_up().await {
        panic!("号段预热失败：{e}");
    }
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        snapshots: None,
        charging_stop: None,
    });
    Some((
        chargepilot_http_api::build_router(deps, prometheus_handle(), None).await,
        Arc::new(db),
        allocator,
    ))
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn req(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path).header(
        "x-real-ip",
        format!("10.{}.1.{}", rand_suffix(), rand_suffix()),
    );
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login_user(router: &axum::Router, tag: &str) -> (String, u64) {
    let (status, body) = req(
        router.clone(),
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

/// 建独立角色 + 权限的管理员并登录，返回 (token, admin_id, role_id)。
async fn login_coupon_admin(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    perms: &[&str],
) -> (String, u64, u64) {
    let username = format!("ma-{}", rand_suffix());
    let (role_id, id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '活动测试角色', 0)",
    )
    .bind(role_id as i64)
    .bind(format!("ma_{username}"))
    .execute(db)
    .await
    .unwrap();
    for p in perms {
        sqlx::query(
            "INSERT INTO `role_permission` (`role_id`, `permission_id`) \
             SELECT ?, `id` FROM `permission` WHERE `code` = ?",
        )
        .bind(role_id as i64)
        .bind(p)
        .execute(db)
        .await
        .unwrap();
    }
    let hash = chargepilot_platform::password::hash("ma-pass-123").unwrap();
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)",
    )
    .bind(id as i64)
    .bind(&username)
    .bind(hash)
    .bind(role_id as i64)
    .execute(db)
    .await
    .unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/login",
        Some(json!({"username": username, "password": "ma-pass-123"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理员登录失败: {body}");
    let token = body["data"]["access_token"].as_str().unwrap().to_string();
    (token, id, role_id)
}

/// 尾部自清理（活动规则 + 券 + 用户 + 管理员/角色）。
async fn cleanup(
    db: &sqlx::MySqlPool,
    coupon_ids: &[u64],
    rule_ids: &[u64],
    user_tag: &str,
    admin_id: u64,
    role_id: u64,
) {
    for rid in rule_ids {
        let _ = sqlx::query(
            "DELETE FROM `audit_log` WHERE `target_type` = 'coupon_activity' AND `target_id` = ?",
        )
        .bind(rid.to_string())
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `coupon_activity_rule` WHERE `id` = ?")
            .bind(*rid as i64)
            .execute(db)
            .await;
    }
    for cid in coupon_ids {
        let _ = sqlx::query("DELETE FROM `coupon_grant_request` WHERE `coupon_id` = ?")
            .bind(*cid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `coupon_grant` WHERE `coupon_id` = ?")
            .bind(*cid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `coupon` WHERE `id` = ?")
            .bind(*cid as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query(
        "DELETE g FROM `coupon_grant` g JOIN `user` u ON u.`id` = g.`user_id` \
         WHERE u.`openid` LIKE ?",
    )
    .bind(format!("development:ma-{user_tag}%"))
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM `payment_order` WHERE `order_no` = ?")
        .bind(format!("P{user_tag}"))
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `openid` LIKE ?")
        .bind(format!("development:ma-{user_tag}%"))
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `audit_log` WHERE `actor_id` = ?")
        .bind(admin_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `admin_user` WHERE `id` = ?")
        .bind(admin_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
        .bind(role_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `role` WHERE `id` = ?")
        .bind(role_id as i64)
        .execute(db)
        .await;
}

/// 按本测试的规则 ID 过滤发放结果（dev 库可能存在其他测试残留的规则命中）。
fn of_rule(
    results: &[chargepilot_mod_marketing::activity::ActivityResult],
    rule_id: u64,
) -> Option<&chargepilot_mod_marketing::activity::ActivityResult> {
    results.iter().find(|r| r.rule_id == rule_id)
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn activity_rule_and_grant_idempotency() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let tag = rand_suffix();
    let (admin_token, admin_id, admin_role) = login_coupon_admin(
        &router,
        &db,
        &alloc,
        &[
            "coupon.read",
            "coupon.create",
            "coupon.update",
            "coupon.activity.read",
            "coupon.activity.manage",
        ],
    )
    .await;
    // 受益人 A、第二受益人 C、第三个用户 D（总量上限用）、邀请人 B（种一笔到账充值）。
    let (_ta, uid_a) = login_user(&router, &format!("ma-{tag}-a")).await;
    let (_tc, uid_c) = login_user(&router, &format!("ma-{tag}-c")).await;
    let (_td, uid_d) = login_user(&router, &format!("ma-{tag}-d")).await;
    let (_tb, uid_b) = login_user(&router, &format!("ma-{tag}-b")).await;
    let month = chrono::Utc::now().date_naive().with_day(1).unwrap();
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, \
         `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) \
         VALUES (?, ?, 'wallet_recharge', 0, ?, 'wechat', 100, 100, 'paid', UTC_TIMESTAMP(3), ?)",
    )
    .bind(alloc.next_id().await.unwrap() as i64)
    .bind(format!("P{tag}"))
    .bind(uid_b as i64)
    .bind(month)
    .execute(db.as_ref())
    .await
    .unwrap();

    // 1. 奖励券 + 门槛活动（max_grants=2, per_user_limit=1）。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupons",
        Some(json!({
            "name": "满50减5奖励券", "discount_type": "amount",
            "discount_value_cents": 500, "min_charge_cents": 1000,
            "valid_hours": 48, "total_quota": 0, "per_user_quota": 5
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建奖励券: {body}");
    let coupon_id = body["data"]["id"].as_u64().unwrap();
    let start = chrono::Utc::now() - chrono::Duration::hours(1);
    let end = chrono::Utc::now() + chrono::Duration::days(7);
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupon-activities",
        Some(json!({
            "name": format!("满减活动{tag}"), "trigger_type": "threshold_redeem",
            "coupon_id": coupon_id.to_string(), "threshold_cents": 5000,
            "max_grants": 2, "per_user_limit": 1,
            "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建活动应成功: {body}");
    let rule_id = body["data"]["id"].as_u64().unwrap();

    // 建规则失败路径：满减门槛为 0、邀请有奖缺邀请人券、结束早于开始、非法触发类型。
    for (i, bad) in [
        json!({"name": "无门槛", "trigger_type": "threshold_redeem", "coupon_id": coupon_id.to_string(), "threshold_cents": 0, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()}),
        json!({"name": "缺邀请券", "trigger_type": "invite_reward", "coupon_id": coupon_id.to_string(), "threshold_cents": 0, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()}),
        json!({"name": "时序倒置", "trigger_type": "threshold_redeem", "coupon_id": coupon_id.to_string(), "threshold_cents": 1, "start_at": end.to_rfc3339(), "end_at": start.to_rfc3339()}),
        json!({"name": "坏触发", "trigger_type": "cashback", "coupon_id": coupon_id.to_string(), "threshold_cents": 0, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()}),
    ]
    .iter()
    .enumerate()
    {
        let (status, body) = req(
            router.clone(),
            "POST",
            "/api/v1/admin/coupon-activities",
            Some(bad.clone()),
            Some(&admin_token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "用例 {i} 应 400: {body}");
    }
    // 奖励券停用 → 建活动 400。
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupons/{coupon_id}"),
        Some(json!({"name": "满50减5奖励券", "status": "disabled"})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupon-activities",
        Some(json!({"name": "停用券活动", "trigger_type": "threshold_redeem", "coupon_id": coupon_id.to_string(), "threshold_cents": 1, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "停用券不可配活动");
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupons/{coupon_id}"),
        Some(json!({"name": "满50减5奖励券", "status": "active"})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 2. 门槛发放：5000 分命中 → 发 1 张；重放同事件键 already；不同事件键被单人上限挡。
    let now = chrono::Utc::now();
    let ev = ActivityEvent {
        trigger_type: chargepilot_domain::marketing::ActivityTriggerType::ThresholdRedeem,
        user_id: uid_a,
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-1"
        )),
        amount_cents: 5000,
        inviter_id: 0,
        now,
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev)
        .await
        .expect("门槛发放应成功");
    let hit = of_rule(&results, rule_id).expect("应命中本测试规则: {results:?}");
    assert!(!hit.already_granted);
    assert_eq!(hit.coupon_id, coupon_id);
    // 重放不新增。
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev)
        .await
        .expect("重放应成功");
    let hit = of_rule(&results, rule_id).expect("重放应命中");
    assert!(hit.already_granted, "重放应标记 already");
    let grant_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `coupon_grant` WHERE `coupon_id` = ? AND `user_id` = ?",
    )
    .bind(coupon_id as i64)
    .bind(uid_a as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(grant_total, 1, "重放不新增券");
    // 金额不足 5000 → 不命中。
    let ev_low = ActivityEvent {
        amount_cents: 4999,
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-low"
        )),
        ..ev.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_low)
        .await
        .expect("低额发放应成功");
    assert!(
        of_rule(&results, rule_id).is_none(),
        "金额不足应跳过: {results:?}"
    );
    // 不同事件键 → 单人上限（1）跳过。
    let ev_2 = ActivityEvent {
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-2"
        )),
        ..ev.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_2)
        .await
        .expect("第二次发放应成功");
    assert!(
        of_rule(&results, rule_id).is_none(),
        "单人上限应跳过: {results:?}"
    );
    // 第二受益人 C：总量上限 2 的第二张。
    let ev_c = ActivityEvent {
        user_id: uid_c,
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-3"
        )),
        ..ev.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_c)
        .await
        .expect("C 的发放应成功");
    assert!(of_rule(&results, rule_id).is_some(), "C 应命中");
    // 总量已满 2：第三个用户被总量上限跳过。
    let ev_d = ActivityEvent {
        user_id: uid_d,
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-4"
        )),
        ..ev.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_d)
        .await
        .expect("D 的发放应成功");
    assert!(
        of_rule(&results, rule_id).is_none(),
        "总量上限应跳过: {results:?}"
    );

    // 3. 管理列表：含活动行与 granted_count=2（activity 来源合计）。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/coupon-activities",
        None,
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "活动列表: {body}");
    let items = body["data"]["items"].as_array().unwrap();
    let row = items
        .iter()
        .find(|x| x["id"] == rule_id)
        .expect("列表应含本活动");
    assert_eq!(row["granted_count"], 2);
    assert_eq!(row["coupon_name"], "满50减5奖励券");
    // 状态筛选。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/coupon-activities?status=active",
        None,
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == rule_id)
    );

    // 4. 更新：停用后不再命中；触发类型不可改；缺失 404。
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupon-activities/{rule_id}"),
        Some(json!({
            "name": format!("满减活动{tag}停用"), "trigger_type": "threshold_redeem",
            "coupon_id": coupon_id.to_string(), "threshold_cents": 5000,
            "max_grants": 2, "per_user_limit": 1, "status": "disabled",
            "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用活动: {body}");
    let ev_5 = ActivityEvent {
        user_id: uid_d,
        event_key: chargepilot_domain::marketing::activity_event_key_for_recharge(&format!(
            "R-{tag}-5"
        )),
        ..ev.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_5)
        .await
        .expect("停用后发放应成功");
    assert!(of_rule(&results, rule_id).is_none(), "停用活动不命中");
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupon-activities/{rule_id}"),
        Some(json!({
            "name": "改触发", "trigger_type": "holiday", "coupon_id": coupon_id.to_string(),
            "threshold_cents": 0, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "触发类型不可改");
    let (status, _) = req(
        router.clone(),
        "PUT",
        "/api/v1/admin/coupon-activities/99999999",
        Some(json!({"name": "不存在", "trigger_type": "holiday", "coupon_id": coupon_id.to_string(), "threshold_cents": 0, "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "缺失规则 404");

    // 5. 邀请奖励：邀请人券 + 被邀请人券；B 已建立（充值入账），B 邀请 D。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupons",
        Some(json!({
            "name": "邀请人奖励券", "discount_type": "amount",
            "discount_value_cents": 300, "min_charge_cents": 0,
            "valid_hours": 24, "total_quota": 0, "per_user_quota": 5
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let inviter_coupon_id = body["data"]["id"].as_u64().unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupon-activities",
        Some(json!({
            "name": format!("邀请有礼{tag}"), "trigger_type": "invite_reward",
            "coupon_id": coupon_id.to_string(), "inviter_coupon_id": inviter_coupon_id.to_string(),
            "threshold_cents": 0, "per_user_limit": 2,
            "start_at": start.to_rfc3339(), "end_at": end.to_rfc3339()
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建邀请活动: {body}");
    let invite_rule_id = body["data"]["id"].as_u64().unwrap();
    // 未建立用户的邀请（C 邀请 D，C 无充值/订单）→ 整条跳过。
    let ev_invite_bad = ActivityEvent {
        trigger_type: chargepilot_domain::marketing::ActivityTriggerType::InviteReward,
        user_id: uid_d,
        event_key: chargepilot_domain::marketing::activity_event_key_for_order(&format!(
            "C-{tag}-bad"
        )),
        amount_cents: 0,
        inviter_id: uid_c,
        now,
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_invite_bad)
        .await
        .expect("未建立邀请应成功跳过");
    assert!(
        of_rule(&results, invite_rule_id).is_none(),
        "邀请人未建立应整条跳过: {results:?}"
    );
    // B（已建立）邀请 D → 双方发券。
    let ev_invite = ActivityEvent {
        trigger_type: chargepilot_domain::marketing::ActivityTriggerType::InviteReward,
        user_id: uid_d,
        event_key: chargepilot_domain::marketing::activity_event_key_for_order(&format!(
            "C-{tag}-1"
        )),
        amount_cents: 0,
        inviter_id: uid_b,
        now,
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_invite)
        .await
        .expect("邀请发放应成功");
    let hit = of_rule(&results, invite_rule_id).expect("邀请应命中");
    assert!(hit.inviter_rewarded, "邀请人应获得奖励");
    // 自邀拒绝。
    let ev_self = ActivityEvent {
        user_id: uid_b,
        event_key: chargepilot_domain::marketing::activity_event_key_for_order(&format!(
            "C-{tag}-self"
        )),
        inviter_id: uid_b,
        ..ev_invite.clone()
    };
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_self)
        .await
        .expect("自邀发放应成功跳过");
    assert!(
        of_rule(&results, invite_rule_id).is_none(),
        "自邀应跳过: {results:?}"
    );
    // 邀请重放：already 且邀请人已发放。
    let results = chargepilot_mod_marketing::activity::apply(db.as_ref(), &ev_invite)
        .await
        .expect("邀请重放应成功");
    let hit = of_rule(&results, invite_rule_id).expect("邀请重放应命中");
    assert!(hit.already_granted);
    assert!(hit.inviter_rewarded, "重放时邀请人奖励已存在");
    let inviter_grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `coupon_grant` WHERE `coupon_id` = ? AND `user_id` = ? \
         AND `grant_source` = 'invite_reward'",
    )
    .bind(inviter_coupon_id as i64)
    .bind(uid_b as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(inviter_grants, 1, "邀请人只一张");

    // 6. 审计落库。
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `actor_id` = ? AND `target_type` = 'coupon_activity'",
    )
    .bind(admin_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        audit_count, 3,
        "建 2 次活动 + 成功更新 1 次各一条审计（失败请求不落审计）: {audit_count}"
    );

    cleanup(
        db.as_ref(),
        &[coupon_id, inviter_coupon_id],
        &[rule_id, invite_rule_id],
        &tag,
        admin_id,
        admin_role,
    )
    .await;
}
