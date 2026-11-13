//! 阶段 05 批 2 主缝集成测试：优惠券建券 → 人工发放（幂等）→ 持券视图 →
//! 过期/停用不参与 → 发放统计。发放条件与请求重放断言走 HTTP（spec 测试决策）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::ApiDeps;
use chargepilot_platform::ids::SegmentAllocator;
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
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return None;
    };
    let Some(redis_url) = env_or_skip("TEST_REDIS_URL") else {
        eprintln!("跳过：TEST_REDIS_URL 未设置");
        return None;
    };
    let db = match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("跳过：数据库连接失败 {e}");
            return None;
        }
    };
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 60));
    if let Err(e) = allocator.warm_up().await {
        eprintln!("跳过：号段预热失败 {e}");
        return None;
    }
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        // 券域无进程内服务组（服务函数直接吃连接池），其余域不装配。
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        charging_stop: None,
    });
    Some((
        chargepilot_http_api::build_router(deps, prometheus_handle(), None),
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

/// 合法 UUID v4（发放请求号须通过服务端校验）。
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

/// 建独立角色 + 券权限的管理员并登录，返回 (token, admin_id, role_id)。
/// 不复用内置角色，避免污染 customer_cs 的权限集合。
async fn login_coupon_admin(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    perms: &[&str],
) -> (String, u64, u64) {
    let username = format!("mk-{}", rand_suffix());
    let (role_id, id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '券域测试角色', 0)",
    )
    .bind(role_id as i64)
    .bind(format!("mk_{username}"))
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
    let hash = chargepilot_platform::password::hash("mk-pass-123").unwrap();
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
        Some(json!({"username": username, "password": "mk-pass-123"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理员登录失败: {body}");
    let token = body["data"]["access_token"].as_str().unwrap().to_string();
    (token, id, role_id)
}

/// 尾部自清理：按测试期记录的 coupon_id / 用户 openid / 管理员与角色主键。
async fn cleanup(
    db: &sqlx::MySqlPool,
    coupon_ids: &[u64],
    user_tag: &str,
    admin_ids: &[u64],
    role_ids: &[u64],
) {
    for cid in coupon_ids {
        let _ = sqlx::query("DELETE FROM `coupon_grant_request` WHERE `coupon_id` = ?")
            .bind(*cid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `coupon_redemption` WHERE `coupon_id` = ?")
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
    let _ = sqlx::query("DELETE FROM `coupon_grant` g JOIN `user` u ON u.`id` = g.`user_id` WHERE u.`openid` LIKE ?")
        .bind(format!("development:mk-{user_tag}%"))
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `user` WHERE `openid` LIKE ?")
        .bind(format!("development:mk-{user_tag}%"))
        .execute(db)
        .await;
    for aid in admin_ids {
        let _ = sqlx::query("DELETE FROM `audit_log` WHERE `actor_id` = ?")
            .bind(*aid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `admin_user` WHERE `id` = ?")
            .bind(*aid as i64)
            .execute(db)
            .await;
    }
    for rid in role_ids {
        let _ = sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
            .bind(*rid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `role` WHERE `id` = ?")
            .bind(*rid as i64)
            .execute(db)
            .await;
    }
}

#[tokio::test]
async fn coupon_create_grant_query_stats() {
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
            "coupon.grant",
        ],
    )
    .await;
    let (ro_token, ro_id, ro_role) =
        login_coupon_admin(&router, &db, &alloc, &["coupon.read"]).await;
    let (user_token, uid) = login_user(&router, &format!("mk-{tag}")).await;

    // 1. 建券（amount 型）：返回自增 id 与强制 active。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupons",
        Some(json!({
            "name": "新客立减5元", "discount_type": "amount",
            "discount_value_cents": 500, "min_charge_cents": 1000,
            "valid_hours": 24, "total_quota": 10, "per_user_quota": 1
        })),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建券应成功: {body}");
    let coupon_id = body["data"]["id"].as_u64().unwrap();
    assert!(coupon_id > 0);
    assert_eq!(body["data"]["status"], "active", "创建强制 active");

    // 校验失败路径：percent=100 / free_minutes=0 / end<=start / 名称空白。
    for (i, bad) in [
        json!({"name": "折扣", "discount_type": "percentage", "discount_percent": 100.0, "min_charge_cents": 0, "valid_hours": 24, "per_user_quota": 1}),
        json!({"name": "时长", "discount_type": "time_free", "free_minutes": 0, "min_charge_cents": 0, "valid_hours": 24, "per_user_quota": 1}),
        json!({"name": "时序", "discount_type": "amount", "discount_value_cents": 100, "min_charge_cents": 0, "valid_hours": 24, "per_user_quota": 1, "start_at": "2026-01-02T00:00:00Z", "end_at": "2026-01-01T00:00:00Z"}),
        json!({"name": "  ", "discount_type": "amount", "discount_value_cents": 100, "min_charge_cents": 0, "valid_hours": 24, "per_user_quota": 1}),
    ]
    .iter()
    .enumerate()
    {
        let (status, body) = req(
            router.clone(),
            "POST",
            "/api/v1/admin/coupons",
            Some(bad.clone()),
            Some(&admin_token),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "用例 {i} 应 400: {body}");
        assert_eq!(body["code"], 1005, "用例 {i} 错误码");
    }

    // 2. 人工发放（幂等）：同 request_id 重放同 grant_id；不同用户冲突。
    let request_id = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": request_id, "user_id": uid.to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "发放应成功: {body}");
    let grant_id = body["data"]["coupon_grant_id"].as_u64().unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": request_id, "user_id": uid.to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放应成功");
    assert_eq!(
        body["data"]["coupon_grant_id"], grant_id,
        "重放复用首次发放"
    );
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": request_id, "user_id": (uid + 1).to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "同请求号不同用户应 409: {body}"
    );
    // 非 UUID 请求号 400。
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": "not-a-uuid", "user_id": uid.to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 发放准入：per_user_quota=1 第二次发放冲突；模板停用后发放冲突。
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": rand_uuid(), "user_id": uid.to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "单人上限应 409");
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupons/{coupon_id}"),
        Some(json!({"name": "新客立减5元", "status": "disabled"})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用应成功");
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": rand_uuid(), "user_id": (uid + 2).to_string()})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "停用模板发放应 409");

    // 3. 持券视图：种一条过期券 + 一条已用券后核对派生与筛选。
    //    过期种子（source_event_id 唯一）。
    let seed_event = format!("mk-seed-{tag}-expired");
    sqlx::query(
        "INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, \
         `expired_at`, `source_event_id`) VALUES (?, ?, 'manual', 'unused', \
         DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 1 HOUR), ?)",
    )
    .bind(coupon_id as i64)
    .bind(uid as i64)
    .bind(&seed_event)
    .execute(db.as_ref())
    .await
    .unwrap();
    // 恢复模板启用后再种一条已用券（人工模拟终态；核销事务属批 4）。
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupons/{coupon_id}"),
        Some(json!({"name": "新客立减5元改", "status": "active"})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "恢复启用应成功");
    let used_event = format!("mk-seed-{tag}-used");
    let used_result = sqlx::query(
        "INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, \
         `expired_at`, `used_at`, `source_event_id`) VALUES (?, ?, 'manual', 'used', \
         DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 24 HOUR), UTC_TIMESTAMP(3), ?)",
    )
    .bind(coupon_id as i64)
    .bind(uid as i64)
    .bind(&used_event)
    .execute(db.as_ref())
    .await
    .unwrap();
    let used_id = used_result.last_insert_id();

    // coupon/my：排序按 status 字典序（expired < unused < used，与旧版一致），
    // 过期由内存派生，usable 标记正确。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupon/my",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "持券列表应成功: {body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "应含三条券: {items:?}");
    assert_eq!(items[0]["status"], "expired", "字典序 expired 在前");
    assert_eq!(items[0]["usable"], false);
    let derived: Vec<&str> = items
        .iter()
        .map(|x| x["status"].as_str().unwrap())
        .collect();
    assert_eq!(derived, vec!["expired", "unused", "used"], "完整排序");
    let unused_item = items.iter().find(|x| x["status"] == "unused").unwrap();
    assert_eq!(unused_item["grant_id"], grant_id);
    assert_eq!(unused_item["discount_type"], "amount");
    assert_eq!(unused_item["discount_value_cents"], 500);
    // 名称来自恢复后的模板（改名生效）。
    assert_eq!(items[0]["name"], "新客立减5元改");

    // status 筛选与 only_usable。
    let (status, _bad) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupon/my?status=usable-not-legal",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法状态筛选应 400");
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupon/my?only_usable=true",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let usable_items = body["data"]["items"].as_array().unwrap();
    assert_eq!(usable_items.len(), 1, "仅一张可用: {usable_items:?}");
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupon/my?status=expired",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["count"], 1, "过期筛选一条");

    // 4. 支付前可用券：停用模板时不含（上面已恢复 active，此时含 1 张）。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupons",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "可用券应成功: {body}");
    let avail = body["data"]["items"].as_array().unwrap();
    assert_eq!(avail.len(), 1, "仅未用未过期一张: {avail:?}");
    assert_eq!(avail[0]["grant_id"], grant_id);
    assert_eq!(avail[0]["min_charge_cents"], 1000);
    // 停用模板 → Available 排除（JOIN status='active'）。
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/coupons/{coupon_id}"),
        Some(json!({"name": "新客立减5元改", "status": "disabled"})),
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/coupons",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["items"].as_array().unwrap().len(),
        0,
        "停用模板的券不参与支付"
    );

    // 5. 发放统计：granted=3（1 unused + 1 used + 1 过期）、usage_rate=1/3。
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/coupons/{coupon_id}/stats"),
        None,
        Some(&admin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "统计应成功: {body}");
    assert_eq!(body["data"]["granted_count"], 3);
    assert_eq!(body["data"]["unused_count"], 1);
    assert_eq!(body["data"]["used_count"], 1);
    assert_eq!(body["data"]["expired_count"], 1);
    assert_eq!(body["data"]["total_quota"], 10);
    let rate = body["data"]["usage_rate"].as_f64().unwrap();
    assert!((rate - 1.0 / 3.0).abs() < 1e-9, "使用率 1/3: {rate}");

    // 6. 权限：只有 read 权限的管理员建券/发放 403。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/coupons",
        Some(json!({"name": "越权", "discount_type": "amount", "discount_value_cents": 100, "min_charge_cents": 0, "valid_hours": 24, "per_user_quota": 1})),
        Some(&ro_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无 create 权限应 403");
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/coupons/{coupon_id}/grants"),
        Some(json!({"request_id": rand_uuid(), "user_id": uid.to_string()})),
        Some(&ro_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无 grant 权限应 403");

    // 7. 审计落库（create/update/grant 同事务）。
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE `actor_id` = ? AND `target_type` = 'coupon' \
         AND `action` IN ('create','update','grant')",
    )
    .bind(admin_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert!(
        audit_count >= 5,
        "create+2 次改状态+2 次发放应各有审计: {audit_count}"
    );

    cleanup(
        db.as_ref(),
        &[coupon_id],
        &tag,
        &[admin_id, ro_id],
        &[admin_role, ro_role],
    )
    .await;
    let _ = user_token;
    let _ = used_id;
}
