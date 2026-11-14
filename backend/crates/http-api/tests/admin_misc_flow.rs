//! 批次补齐端点主缝集成测试（admin_misc）：反馈工单（列表/回复/关闭 + 审计留痕）、
//! 审计日志分页筛选、本人与管理员 MFA 设置、方案预览试算与设备能力。
//! 契约基准：旧仓库 casework.go / audit_log.go / admin_mfa_settings.go /
//! account_management.go(MFA) / charging_schemes.go(preview、executionCapabilities)。

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

async fn build() -> Option<(axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>)> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let db_url = env_or_skip("TEST_MYSQL_URL")?;
    let redis_url = env_or_skip("TEST_REDIS_URL")?;
    let db = chargepilot_storage::open_pool(&db_url, 8).await.ok()?;
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 50));
    allocator.warm_up().await.ok()?;
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        // 本 flow 不触达钱包与支付域。
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
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

fn rand_ip() -> String {
    format!("10.{}.1.{}", rand_suffix(), rand_suffix())
}

/// 跨测试文件互斥：共享开发库，用固定端口 TCP 监听串行各 flow 文件。
fn global_flow_lock() -> std::net::TcpListener {
    loop {
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:39701") {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// 本文件内多个断言共享种子，进程级互斥防止 tokio 并行测试互相干扰。
static MISC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn req(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    ip: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-real-ip", ip);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
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

async fn login(router: &axum::Router, username: &str, password: &str) -> Option<String> {
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/login",
        Some(&format!(
            r#"{{"username":"{username}","password":"{password}"}}"#
        )),
        None,
        &rand_ip(),
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("登录失败: {body}");
        return None;
    }
    body["data"]["access_token"].as_str().map(str::to_string)
}

/// SQL 直建受限管理员（角色权限 = 指定权限码），返回 (用户名, 账号 ID)。
async fn seed_limited_admin(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    suffix: &str,
    password: &str,
    permissions: &[&str],
) -> (String, u64) {
    let username = format!("misc-{suffix}");
    let (role_id, admin_id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '零散端点测试角色', 0)",
    )
    .bind(role_id)
    .bind(format!("misc_{suffix}"))
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
    let hash = chargepilot_platform::password::hash(password).unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)")
        .bind(admin_id)
        .bind(&username)
        .bind(hash)
        .bind(role_id)
        .execute(db)
        .await
        .unwrap();
    (username, admin_id)
}

/// 种子：一条用户反馈（pending / processed / closed 任一初始状态），返回 (反馈 ID, 用户 ID)。
async fn seed_feedback(db: &sqlx::MySqlPool, alloc: &SegmentAllocator, status: &str) -> (u64, u64) {
    let user_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '反馈测试用户', 'active')")
        .bind(user_id as i64)
        .bind(format!("misc-{user_id}"))
        .execute(db)
        .await
        .unwrap();
    let id = alloc.next_id().await.unwrap();
    sqlx::query(
        "INSERT INTO `feedback` (`id`, `user_id`, `rating`, `category`, `content`, `images_json`, `status`) \
         VALUES (?, ?, 5, 'complaint', '充电桩屏幕反光', CAST(? AS JSON), ?)",
    )
    .bind(id as i64)
    .bind(user_id as i64)
    .bind(json!(["https://img.example.com/a.jpg"]).to_string())
    .bind(status)
    .execute(db)
    .await
    .unwrap();
    (id, user_id)
}

/// 种子：直接写一条审计记录（指定 module/action），用于列表筛选断言。
async fn seed_audit(db: &sqlx::MySqlPool, alloc: &SegmentAllocator, module: &str, action: &str) {
    let actor_id = alloc.next_id().await.unwrap();
    use chrono::Datelike as _;
    let month = chrono::Utc::now().date_naive().with_day(1).unwrap();
    sqlx::query(
        "INSERT INTO `audit_log` (`actor_id`, `actor_name`, `module`, `action`, `target_type`, `target_id`, `client_ip`, `created_month`) \
         VALUES (?, '种子账号', ?, ?, 'seed', '', '10.0.0.1', ?)",
    )
    .bind(actor_id as i64)
    .bind(module)
    .bind(action)
    .bind(month)
    .execute(db)
    .await
    .unwrap();
}

/// 预览用方案（与结算 flow 同参数）：amount 套餐 100 分，电价 50+30 分/kWh。
fn preview_scheme() -> Value {
    json!({
        "name": "预览测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [
            {"id": 1, "name": "1元", "mode": "amount", "price_cents": 100},
            {"id": 2, "name": "5小时", "mode": "duration", "price_cents": 50, "minutes": 300}
        ],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 2, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    })
}

/// 预览用计量：600 秒 200Wh（应计 16 分：电费 10 + 服务费 6）。
fn preview_meter() -> Value {
    let start = "2026-09-30T00:00:00+08:00";
    let end = "2026-09-30T00:10:00+08:00";
    json!({
        "started_at": start,
        "ended_at": end,
        "charged_seconds": 600,
        "charged_wh": 200,
        "segments": [{"started_at": start, "ended_at": end, "energy_wh": 200, "peak_w": 1500}]
    })
}

/// 尾部清理：按主键删除种子（审计与流水保留供追溯的规则不适用于测试种子，一并删除）。
async fn cleanup(
    db: &sqlx::MySqlPool,
    feedback_ids: &[u64],
    audit_modules: &[&str],
    user_ids: &[u64],
    admin_ids: &[u64],
    role_ids: &[u64],
    device_id: &str,
    station_id: u64,
) {
    for id in feedback_ids {
        let _ = sqlx::query("DELETE FROM `feedback` WHERE `id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
    }
    for module in audit_modules {
        let _ =
            sqlx::query("DELETE FROM `audit_log` WHERE `module` = ? AND `actor_name` = '种子账号'")
                .bind(module)
                .execute(db)
                .await;
    }
    for uid in user_ids {
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(*uid as i64)
            .execute(db)
            .await;
    }
    for aid in admin_ids {
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
    let _ = sqlx::query("DELETE FROM `device_meta` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
        .bind(station_id as i64)
        .execute(db)
        .await;
}

#[tokio::test]
async fn misc_endpoints_flow() {
    let _flow_lock = global_flow_lock();
    let _guard = MISC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let suf = rand_suffix();
    let pass = "misc-pass-12345";

    // 种子：全权限超管 + 受限号 A（本次端点全部权限）+ 受限号 B（仅 feedback.read，越权与被管理对象）。
    let super_name = format!("root-{suf}");
    let super_id = alloc.next_id().await.unwrap();
    let super_hash = chargepilot_platform::password::hash("super-pass-123").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(super_id)
        .bind(&super_name)
        .bind(super_hash)
        .execute(db.as_ref())
        .await
        .unwrap();
    let (a_name, a_id) = seed_limited_admin(
        db.as_ref(),
        &alloc,
        &format!("a{suf}"),
        pass,
        &[
            "feedback.read",
            "feedback.reply",
            "audit.read",
            "pricing.read",
        ],
    )
    .await;
    let (b_name, b_id) = seed_limited_admin(
        db.as_ref(),
        &alloc,
        &format!("b{suf}"),
        pass,
        &["feedback.read"],
    )
    .await;
    let a_role: u64 = sqlx::query_scalar("SELECT `role_id` FROM `admin_user` WHERE `id` = ?")
        .bind(a_id)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    let b_role: u64 = sqlx::query_scalar("SELECT `role_id` FROM `admin_user` WHERE `id` = ?")
        .bind(b_id)
        .fetch_one(db.as_ref())
        .await
        .unwrap();

    let super_token = login(&router, &super_name, "super-pass-123")
        .await
        .expect("超管登录");
    let a_token = login(&router, &a_name, pass).await.expect("受限A登录");
    let b_token = login(&router, &b_name, pass).await.expect("受限B登录");

    // 反馈与审计种子。
    let (fb_pending_1, fb_user_1) = seed_feedback(db.as_ref(), &alloc, "pending").await;
    let (fb_pending_2, fb_user_2) = seed_feedback(db.as_ref(), &alloc, "pending").await;
    let (fb_closed, fb_user_3) = seed_feedback(db.as_ref(), &alloc, "closed").await;
    seed_audit(db.as_ref(), &alloc, "coupon", "grant").await;
    seed_audit(db.as_ref(), &alloc, "station", "update").await;

    // 1. 反馈列表：形状（id 字符串、images 数组）+ 状态筛选 + 越权。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/feedback?page=1&page_size=10",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "反馈列表应成功: {body}");
    assert_eq!(body["code"], 0);
    assert!(
        body["data"]["total"].as_i64().unwrap() >= 3,
        "至少 3 条种子: {body}"
    );
    let first = &body["data"]["items"][0];
    assert!(first["id"].is_string(), "id 应为字符串: {first}");
    assert!(first["user_id"].is_string(), "user_id 应为字符串: {first}");
    assert!(first["images"].is_array(), "images 应为数组: {first}");
    // ORDER BY id DESC：最后种的 fb_closed 应排第一。
    assert_eq!(first["status"].as_str().unwrap(), "closed");

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/feedback?status=pending",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pending_total = body["data"]["total"].as_i64().unwrap();
    assert!(pending_total >= 2, "pending 种子至少 2 条: {body}");
    for item in body["data"]["items"].as_array().unwrap() {
        assert_eq!(item["status"], "pending", "筛选后只能有 pending: {item}");
    }

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/feedback?status=bogus",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "非法状态应 400: {body}");

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/feedback",
        None,
        Some(&b_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "b 有 feedback.read 应可读: {body}");

    // 2. 反馈回复：pending→processed + 审计留痕；重复回复 409；关闭与重复关闭。
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/feedback/{fb_pending_1}/reply"),
        Some(r#"{"action":"reply","reply_content":"已安排更换屏幕"}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "回复应成功: {body}");
    assert_eq!(body["data"]["saved"], true);
    let (fb_status, reply_content, replied_by): (String, String, u64) = sqlx::query_as(
        "SELECT `status`, `reply_content`, `replied_by` FROM `feedback` WHERE `id` = ?",
    )
    .bind(fb_pending_1 as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(fb_status, "processed", "回复后应 processed");
    assert_eq!(reply_content, "已安排更换屏幕");
    assert_eq!(replied_by, a_id, "回复人应为操作账号");
    let (audit_module, audit_action): (String, String) = sqlx::query_as(
        "SELECT `module`, `action` FROM `audit_log` \
         WHERE `module` = 'feedback' AND `target_id` = ? ORDER BY `id` DESC LIMIT 1",
    )
    .bind(fb_pending_1.to_string())
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (audit_module.as_str(), audit_action.as_str()),
        ("feedback", "reply")
    );

    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/feedback/{fb_pending_1}/reply"),
        Some(r#"{"action":"reply","reply_content":"再次回复"}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "processed 再回复应 409: {body}"
    );

    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/feedback/{fb_pending_1}/reply"),
        Some(r#"{"action":"reply","reply_content":""}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "空回复应 400: {body}");

    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/feedback/{fb_pending_2}/reply"),
        Some(r#"{"action":"close"}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "关闭应成功");
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/feedback/{fb_pending_2}/reply"),
        Some(r#"{"action":"close"}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重复关闭应静默成功");
    let closed_status: String =
        sqlx::query_scalar("SELECT `status` FROM `feedback` WHERE `id` = ?")
            .bind(fb_pending_2 as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(closed_status, "closed");

    // 3. 审计日志：分页、模块筛选、操作人筛选、时间边界、坏时间。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/audit-logs?page=1&page_size=2",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "审计列表应成功: {body}");
    assert!(
        body["data"]["total"].as_i64().unwrap() >= 4,
        "至少种子 2 + 反馈审计 2 条: {body}"
    );
    assert_eq!(
        body["data"]["items"].as_array().unwrap().len(),
        2,
        "page_size=2"
    );
    let row = &body["data"]["items"][0];
    assert!(row["id"].is_u64(), "id 应为数字: {row}");
    assert!(row["actor_id"].is_u64(), "actor_id 应为数字: {row}");
    assert!(row["before_json"].is_null() || row["before_json"].is_string());

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/audit-logs?module=feedback",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for item in body["data"]["items"].as_array().unwrap() {
        assert_eq!(
            item["module"], "feedback",
            "模块筛选后只能 feedback: {item}"
        );
        assert!(
            item["action"] == "reply" || item["action"] == "close",
            "反馈审计动作应为 reply/close: {item}"
        );
    }

    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/audit-logs?actor={a_name}"),
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let total_by_actor = body["data"]["total"].as_i64().unwrap();
    assert!(total_by_actor >= 1, "按操作人筛选应命中反馈审计: {body}");
    for item in body["data"]["items"].as_array().unwrap() {
        assert_eq!(item["actor_name"], a_name);
    }

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/audit-logs?from=bad-time",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "坏时间格式应 400: {body}");

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/audit-logs?from=2030-01-01T00:00:00Z",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 0, "未来起点应无记录");

    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/audit-logs",
        None,
        Some(&b_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无 audit.read 应 403");

    // 4. 本人 MFA 设置：GET 状态 → enrol（错密码/成功）→ confirm（坏码/对码）→
    // 会话失效 → 重新登录（MFA 挑战链）→ disable。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/auth/mfa-settings",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "MFA 状态应可读: {body}");
    assert_eq!(body["data"]["mfa_enabled"], false);

    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa-settings",
        Some(r#"{"action":"enrol","password":"wrong-password"}"#),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "错密码应 400: {body}");
    assert_eq!(body["code"], 1002, "密码错误属校验失败: {body}");

    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa-settings",
        Some(&format!(r#"{{"action":"enrol","password":"{pass}"}}"#)),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enrol 应成功: {body}");
    let enrollment_id = body["data"]["enrollment_id"].as_str().unwrap().to_string();
    let secret = body["data"]["secret"].as_str().unwrap().to_string();
    assert_eq!(enrollment_id.len(), 43, "绑定标识应为 43 字符");
    assert!(
        body["data"]["otpauth_uri"]
            .as_str()
            .unwrap()
            .contains("otpauth://totp/")
    );
    assert!(body["data"]["expires_in"].as_u64().unwrap() > 0);
    assert_eq!(body["data"]["mfa_enabled"], false);

    let bad_code = "000000".to_string();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa-settings",
        Some(
            &json!({"action":"confirm","enrollment_id":enrollment_id,"code":bad_code}).to_string(),
        ),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "坏码应 400: {body}");

    let now = chrono::Utc::now().timestamp();
    let confirm_code =
        chargepilot_platform::totp::generate(&secret, (now / 30) * 30).expect("TOTP 生成");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa-settings",
        Some(
            &json!({"action":"confirm","enrollment_id":enrollment_id,"code":confirm_code})
                .to_string(),
        ),
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm 应成功: {body}");
    assert_eq!(body["data"]["mfa_enabled"], true);
    assert_eq!(body["data"]["sessions_revoked"], true);
    let mfa_on: bool = sqlx::query_scalar("SELECT `mfa_enabled` FROM `admin_user` WHERE `id` = ?")
        .bind(a_id)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert!(mfa_on, "confirm 后应启用 MFA");
    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/auth/me",
        None,
        Some(&a_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "启用后旧会话应失效");

    // 重新登录：密码 → MFA 挑战 → 新时间步验证码。
    // confirm 已消费当前步（防重放），登录链须等验证器切换到下一窗口。
    let next_window = ((now / 30) + 1) * 30;
    let wait = (next_window - chrono::Utc::now().timestamp() + 1).max(1) as u64;
    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/login",
        Some(&format!(r#"{{"username":"{a_name}","password":"{pass}"}}"#)),
        None,
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "MFA 账号登录应返回挑战: {body}");
    let challenge = body["data"]["mfa_challenge"]
        .as_str()
        .expect("挑战码")
        .to_string();
    let login_code = chargepilot_platform::totp::generate(&secret, next_window).expect("TOTP 生成");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa",
        Some(&json!({"mfa_challenge":challenge,"code":login_code}).to_string()),
        None,
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "MFA 验证应通过: {body}");
    let a_token2 = body["data"]["access_token"]
        .as_str()
        .expect("新令牌")
        .to_string();

    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/auth/mfa-settings",
        None,
        Some(&a_token2),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["mfa_enabled"], true,
        "重新登录后状态应为已启用"
    );

    // disable：需密码 + 未消费步验证码（confirm 消费了 now 步，用下一步）。
    let disable_code =
        chargepilot_platform::totp::generate(&secret, ((now / 30) + 2) * 30).expect("TOTP 生成");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/mfa-settings",
        Some(&json!({"action":"disable","password":pass,"code":disable_code}).to_string()),
        Some(&a_token2),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "disable 应成功: {body}");
    assert_eq!(body["data"]["sessions_revoked"], true);
    let mfa_off: bool = sqlx::query_scalar("SELECT `mfa_enabled` FROM `admin_user` WHERE `id` = ?")
        .bind(a_id)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert!(!mfa_off, "disable 后应关闭 MFA");
    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/auth/mfa-settings",
        None,
        Some(&a_token2),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "disable 后旧会话应失效");

    // 5. 管理端 MFA：本人操作 409；对他人 enrol → confirm → disable。
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/admin-users/{a_id}/mfa"),
        Some(r#"{"action":"disable"}"#),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重置未启用账号应静默成功: {body}");

    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/admin-users/{super_id}/mfa"),
        Some(r#"{"action":"enrol"}"#),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "本人操作应 409: {body}");

    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/admin-users/{b_id}/mfa"),
        Some(r#"{"action":"enrol"}"#),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理端 enrol 应成功: {body}");
    let m_secret = body["data"]["secret"].as_str().unwrap().to_string();
    assert!(
        body["data"]["code"].as_str().is_some(),
        "登记响应应含当前验证码"
    );
    assert_eq!(body["data"]["enabled"], false);
    let m_code = chargepilot_platform::totp::generate(&m_secret, chrono::Utc::now().timestamp())
        .expect("TOTP 生成");
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/admin-users/{b_id}/mfa"),
        Some(&json!({"action":"confirm","secret":m_secret,"code":m_code}).to_string()),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理端 confirm 应成功: {body}");
    assert_eq!(body["data"]["mfa_enabled"], true);
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/admin-users/{b_id}/mfa"),
        Some(r#"{"action":"disable"}"#),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理端 disable 应成功: {body}");
    assert_eq!(body["data"]["mfa_enabled"], false);

    // 管理端 confirm/disable 递增认证版本撤销目标账号旧会话（旧版 bumpAuthVersion 语义）。
    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/auth/mfa-settings",
        None,
        Some(&b_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "被管理后目标旧会话应失效");

    // 6. 方案预览：普通结算、启动失败场景、坏方案。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/settings/charging-schemes/preview",
        Some(
            &json!({"scheme": preview_scheme(), "package_id": 1, "meter": preview_meter(), "scenario": "", "wallet_cents": 1000})
                .to_string(),
        ),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "预览应成功: {body}");
    assert_eq!(body["data"]["status"], "calculated");
    assert_eq!(body["data"]["paid_cents"], 100);
    assert_eq!(
        body["data"]["settlement"]["total_cents"], 16,
        "200Wh 应计 16 分"
    );
    assert_eq!(body["data"]["refund_cents"], 84, "余款 84 分");
    assert_eq!(body["data"]["package"]["id"], 1);

    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/settings/charging-schemes/preview",
        Some(
            &json!({"scheme": preview_scheme(), "package_id": 1, "meter": preview_meter(), "scenario": "start_failed", "wallet_cents": 0})
                .to_string(),
        ),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["refund_cents"], 100, "未启动应全额退款");
    assert_eq!(body["data"]["reason"], "明确未启动，全额退款");

    let mut broken = preview_scheme();
    broken["name"] = json!("");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/settings/charging-schemes/preview",
        Some(
            &json!({"scheme": broken, "package_id": 1, "meter": preview_meter(), "scenario": ""})
                .to_string(),
        ),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "坏方案应 400: {body}");
    assert!(
        body["message"].as_str().unwrap().contains("方案名称"),
        "文案应对齐领域校验: {body}"
    );

    // 7. 设备能力：DC589 固定能力 + 参数校验 + 未知设备。
    let station_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '能力测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let device_id = format!("misc-dev-{}", rand_suffix());
    sqlx::query(
        "INSERT INTO `device_meta` (`id`, `device_id`, `station_id`, `protocol_adapter`, `status`) \
         VALUES (?, ?, ?, 'dc589', 'enabled')",
    )
    .bind(alloc.next_id().await.unwrap() as i64)
    .bind(&device_id)
    .bind(station_id as i64)
    .execute(db.as_ref())
    .await
    .unwrap();

    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/settings/device-capabilities?station_id={station_id}&device_id={device_id}"),
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "能力查询应成功: {body}");
    assert_eq!(body["data"]["protocol_adapter"], "dc589");
    let cap = &body["data"]["capabilities"];
    assert_eq!(cap["max_minutes"], 4320);
    for key in [
        "duration",
        "energy",
        "online_card",
        "stop_when_full",
        "reports_energy",
        "reports_segmented_power",
    ] {
        assert_eq!(cap[key], true, "DC589 应支持 {key}");
    }
    assert_eq!(body["data"]["reports_energy"], true);

    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/settings/device-capabilities?device_id=abcdefgh",
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "缺 station_id 应 400");

    let (status, _) = req(
        router.clone(),
        "GET",
        &format!(
            "/api/v1/admin/settings/device-capabilities?station_id={station_id}&device_id=short"
        ),
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "设备号过短应 400");

    let (status, _) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/settings/device-capabilities?station_id={station_id}&device_id=nonexist01"),
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "未知设备应 404");

    cleanup(
        db.as_ref(),
        &[fb_pending_1, fb_pending_2, fb_closed],
        &["coupon", "station"],
        &[fb_user_1, fb_user_2, fb_user_3],
        &[super_id, a_id, b_id],
        &[a_role, b_role],
        &device_id,
        station_id,
    )
    .await;
    // 回复/关闭产生的 feedback 审计与 MFA 审计（按操作账号定位测试种子）。
    let _ = sqlx::query(
        "DELETE FROM `audit_log` WHERE `module` = 'feedback' AND `target_type` = 'feedback' \
         AND `target_id` IN (?, ?, ?)",
    )
    .bind(fb_pending_1.to_string())
    .bind(fb_pending_2.to_string())
    .bind(fb_closed.to_string())
    .execute(db.as_ref())
    .await;
    let _ = sqlx::query(
        "DELETE FROM `audit_log` WHERE `module` = 'admin_user' AND `action` LIKE 'mfa_%' \
         AND `target_id` IN (?, ?)",
    )
    .bind(a_id.to_string())
    .bind(b_id.to_string())
    .execute(db.as_ref())
    .await;
}
