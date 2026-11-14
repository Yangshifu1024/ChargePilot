//! 阶段 02 主缝集成测试：后台登录链路（真 MySQL + 真 Redis）。
//! 覆盖：bootstrap、密码登录、MFA 挑战/校验/重放拒绝、会话轮换、实时鉴权、错误映射。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::ApiDeps;
use chargepilot_platform::ids::SegmentAllocator;
use http_body_util::BodyExt;
use serde_json::Value;
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

async fn build_app() -> Option<axum::Router> {
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
        allocator,
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        // 这些 flow 不触达钱包与支付域。
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        snapshots: None,
        charging_stop: None,
    });
    let _ = db;
    Some(chargepilot_http_api::build_router(deps, prometheus_handle(), None).await)
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 每个测试用独立随机 IP，避免共享频次窗口。
fn rand_ip() -> String {
    format!("10.{}.{}.{}", rand_suffix(), "1", rand_suffix())
}

/// 直接建后台账号（绕过 HTTP，密码与 MFA 状态自定）。
async fn seed_admin(
    db: &sqlx::MySqlPool,
    allocator: &SegmentAllocator,
    username: &str,
    password: &str,
    mfa: bool,
) -> (String, Option<String>) {
    let id = allocator.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash(password).unwrap();
    let secret = if mfa {
        let s = chargepilot_platform::totp::random_secret();
        sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `mfa_enabled`, `mfa_secret`) VALUES (?, ?, ?, 1, ?)")
            .bind(id).bind(username).bind(&hash).bind(&s)
            .execute(db).await.unwrap();
        Some(s)
    } else {
        sqlx::query(
            "INSERT INTO `admin_user` (`id`, `username`, `password_hash`) VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(username)
        .bind(&hash)
        .execute(db)
        .await
        .unwrap();
        None
    };
    (username.to_string(), secret)
}

async fn post(router: axum::Router, path: &str, body: &str, ip: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-real-ip", ip)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_bearer(
    router: axum::Router,
    path: &str,
    token: &str,
    ip: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("x-real-ip", ip)
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 跨测试文件互斥锁：这些 flow 共享开发库，部分断言依赖全局计数
/// （如"最后一个客户管理员"），用固定端口的 TCP 监听作锁串行各文件。
fn global_flow_lock() -> std::net::TcpListener {
    loop {
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:39701") {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn plain_login_flow() {
    let _flow_lock = global_flow_lock();
    let Some(router) = build_app().await else {
        return;
    };
    let db = chargepilot_storage::open_pool(&env_or_skip("TEST_MYSQL_URL").unwrap(), 4)
        .await
        .unwrap();
    let alloc = Arc::new(SegmentAllocator::new(db.clone(), 20));
    alloc.warm_up().await.unwrap();
    let user = format!("adm-{}", rand_suffix());
    seed_admin(&db, &alloc, &user, "plain-pass-123", false).await;
    let ip = rand_ip();

    // 1. 密码登录（无 MFA）→ 完整回执：profile 平铺 + 双令牌键
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/login",
        &format!(r#"{{"username":"{user}","password":"plain-pass-123"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "登录应成功: {body}");
    assert_eq!(body["data"]["username"], user.as_str());
    assert!(body["data"]["access_token"].is_string());
    assert_eq!(body["data"]["access_token"], body["data"]["token"]);
    assert_eq!(body["data"]["expires_in"], 28800);
    assert_eq!(
        body["data"]["permissions"],
        Value::Array(vec![]),
        "无角色账号权限为空"
    );
    let token = body["data"]["access_token"].as_str().unwrap().to_string();
    let refresh = body["data"]["refresh_token"].as_str().unwrap().to_string();

    // 2. 实时鉴权 me → profile
    let (status, body) = get_bearer(router.clone(), "/api/v1/admin/auth/me", &token, &ip).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["username"], user.as_str());

    // 3. 错误密码 → 401/1001（不泄露存在性文案）
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/login",
        &format!(r#"{{"username":"{user}","password":"wrong-pass-123"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], 1001);

    // 4. refresh 轮换 → 新令牌；旧令牌失效
    let (status, body) = post(router.clone(), "/api/v1/admin/auth/refresh", "null", &ip).await;
    let _ = (status, body);
    let router2 = router.clone();
    let (status, body) = refresh_with(router2, &refresh, &ip).await;
    assert_eq!(status, StatusCode::OK, "刷新应成功: {body}");
    let new_refresh = body["data"]["refresh_token"].as_str().unwrap().to_string();
    let (status, body) = refresh_with(router.clone(), &refresh, &ip).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "旧刷新令牌应失效: {body}");
    let (status, _body) = post(
        router.clone(),
        "/api/v1/admin/auth/login",
        &format!(r#"{{"username":"{user}","password":"plain-pass-123"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _ = new_refresh;
}

async fn refresh_with(router: axum::Router, token: &str, ip: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/admin/auth/refresh")
        .header("authorization", format!("Bearer {token}"))
        .header("x-real-ip", ip)
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn mfa_login_flow_with_replay_rejection() {
    let _flow_lock = global_flow_lock();
    let Some(router) = build_app().await else {
        return;
    };
    let db = chargepilot_storage::open_pool(&env_or_skip("TEST_MYSQL_URL").unwrap(), 4)
        .await
        .unwrap();
    let alloc = Arc::new(SegmentAllocator::new(db.clone(), 20));
    alloc.warm_up().await.unwrap();
    let user = format!("mfa-{}", rand_suffix());
    let (_u, secret) = seed_admin(&db, &alloc, &user, "mfa-pass-123456", true).await;
    let secret = secret.unwrap();
    let ip = rand_ip();

    // 1. 密码通过 → 返回 5 分钟挑战
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/login",
        &format!(r#"{{"username":"{user}","password":"mfa-pass-123456"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge = body["data"]["mfa_challenge"]
        .as_str()
        .expect("应返回挑战")
        .to_string();
    assert!(
        body["data"]["access_token"].is_null(),
        "MFA 账号在验证前不签发会话"
    );

    // 2. 错误验证码 → 401/1001
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/mfa",
        &format!(r#"{{"mfa_challenge":"{challenge}","code":"000000"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["message"], "验证码无效或已过期");

    // 3. 正确验证码 → 完整回执
    let now = chrono::Utc::now().timestamp();
    let code = chargepilot_platform::totp::generate(&secret, now).unwrap();
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/mfa",
        &format!(r#"{{"mfa_challenge":"{challenge}","code":"{code}"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "MFA 完成应成功: {body}");
    assert_eq!(body["data"]["username"], user.as_str());
    let token = body["data"]["access_token"].as_str().unwrap().to_string();

    // 4. 同一挑战再次完成 → 挑战已消费
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/mfa",
        &format!(r#"{{"mfa_challenge":"{challenge}","code":"{code}"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["message"], "登录已失效，请重新登录");

    // 5. 同一验证码（新挑战 + 同步长）→ mfa_last_step 防重放拒绝
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/login",
        &format!(r#"{{"username":"{user}","password":"mfa-pass-123456"}}"#),
        &ip,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge2 = body["data"]["mfa_challenge"].as_str().unwrap().to_string();
    let (status, body) = post(
        router.clone(),
        "/api/v1/admin/auth/mfa",
        &format!(r#"{{"mfa_challenge":"{challenge2}","code":"{code}"}}"#),
        &ip,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "已消费时间步应被拒绝: {body}"
    );
    assert_eq!(body["message"], "该验证码已被使用，请等待下一个验证码");

    // 6. me 可用
    let (status, body) = get_bearer(router, "/api/v1/admin/auth/me", &token, &ip).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["mfa_enabled"], true);
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn login_rate_limit() {
    let _flow_lock = global_flow_lock();
    let Some(router) = build_app().await else {
        return;
    };
    let ip = rand_ip();
    let body = r#"{"username":"ghost-user","password":"whatever-pass-1"}"#;
    let mut last = (StatusCode::OK, Value::Null);
    for i in 0..7 {
        last = post(router.clone(), "/api/v1/admin/auth/login", body, &ip).await;
        eprintln!("第 {i} 次尝试 -> {} code={}", last.0, last.1["code"]);
        if i < 5 {
            assert_eq!(
                last.0,
                StatusCode::UNAUTHORIZED,
                "前 5 次为凭据错误: {last:?}"
            );
        }
    }
    assert_eq!(
        last.0,
        StatusCode::TOO_MANY_REQUESTS,
        "第 6 次起限频: {last:?}"
    );
    assert_eq!(last.1["code"], 4291);
}
