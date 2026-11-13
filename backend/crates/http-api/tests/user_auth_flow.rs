//! 阶段 02 主缝集成测试：C 端登录全链路（真 MySQL + 真 Redis）。
//! 断言只看 HTTP 响应（信封、码值、形状），不看内部实现。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::{ApiDeps, exchanger_from_env};
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

async fn build_app() -> Option<(axum::Router, String, Arc<SegmentAllocator>)> {
    let db_url = env_or_skip("TEST_MYSQL_URL")?;
    let redis_url = env_or_skip("TEST_REDIS_URL")?;
    let db = chargepilot_storage::open_pool(&db_url, 8).await.ok()?;
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 50));
    allocator.warm_up().await.ok()?;
    let deps = Arc::new(ApiDeps {
        db,
        redis_url: Arc::new(redis_url.clone()),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(exchanger_from_env().unwrap()),
    });
    let router = chargepilot_http_api::build_router(deps, prometheus_handle());
    let account = format!("it-{}", rand_suffix());
    Some((router, account, allocator))
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn post_json(
    router: axum::Router,
    path: &str,
    body: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(t) = bearer {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let resp = router
        .oneshot(
            builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    take(resp).await
}

async fn get_with_bearer(router: axum::Router, path: &str, token: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    take(resp).await
}

async fn take(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn dev_login_full_flow() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some((router, account, _alloc)) = build_app().await else {
        return;
    };
    let code = format!("dev:{account}");

    // 1. 首次登录：200，平铺 6 键，user_id 为字符串
    let (status, body) = post_json(
        router.clone(),
        "/api/v1/user/auth/login",
        &format!(r#"{{"code":"{code}"}}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "首登应成功: {body}");
    assert_eq!(body["code"], 0);
    assert!(body["data"]["jwt"].is_string());
    assert_eq!(
        body["data"]["token"], body["data"]["jwt"],
        "token 为 jwt 兼容别名"
    );
    assert!(body["data"]["refresh_token"].is_string());
    assert_eq!(body["data"]["jwt_expires_in"], 900);
    assert!(body["data"]["user_id"].is_string());
    assert_eq!(body["data"]["is_new_user"], true);
    let jwt = body["data"]["jwt"].as_str().unwrap().to_string();
    let refresh = body["data"]["refresh_token"].as_str().unwrap().to_string();
    let user_id = body["data"]["user_id"].as_str().unwrap().to_string();

    // 2. 二次登录：老用户
    let (status, body) = post_json(
        router.clone(),
        "/api/v1/user/auth/login",
        &format!(r#"{{"code":"{code}"}}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["is_new_user"], false);
    assert_eq!(body["data"]["user_id"], user_id.as_str());

    // 3. profile：钱包零余额（登录建户同事务）
    let (status, body) = get_with_bearer(router.clone(), "/api/v1/user/profile", &jwt).await;
    assert_eq!(status, StatusCode::OK, "profile 应成功: {body}");
    assert_eq!(body["data"]["user_id"], user_id.as_str());
    assert_eq!(body["data"]["phone_bound"], false);
    assert_eq!(body["data"]["wallet"]["status"], "active");
    assert_eq!(body["data"]["wallet"]["available_cents"], 0);

    // 4. refresh 轮换：新对下发
    let (status, body) = post_json(
        router.clone(),
        "/api/v1/user/auth/refresh",
        "null",
        Some(&refresh),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refresh 应成功: {body}");
    let new_refresh = body["data"]["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, refresh, "轮换必须下发新刷新令牌");

    // 5. 旧刷新令牌失效（轮换后旧摘要不再匹配当前 digest）
    let (status, body) = post_json(
        router.clone(),
        "/api/v1/user/auth/refresh",
        "null",
        Some(&refresh),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], 1001);
    assert_eq!(body["message"], "refresh token invalid");

    // 6. logout 新令牌 → 旧 JWT 的会话随之消失
    let (status, _) = post_json(
        router.clone(),
        "/api/v1/user/auth/logout",
        "null",
        Some(&new_refresh),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get_with_bearer(router, "/api/v1/user/profile", &jwt).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["message"], "session expired", "JWT 有效但会话已注销");
}

#[tokio::test]
async fn invalid_code_maps_to_wechat_failure() {
    let Some((router, _, _)) = build_app().await else {
        return;
    };
    let (status, body) = post_json(
        router,
        "/api/v1/user/auth/login",
        r#"{"code":"not-a-dev-code"}"#,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], 3001);
    assert_eq!(body["message"], "WeChat login failed");
}

#[tokio::test]
async fn forged_jwt_is_rejected() {
    let Some((router, account, _)) = build_app().await else {
        return;
    };
    // 用错误密钥伪造（正确形状、错误签名）
    let forged = chargepilot_platform::jwt::sign(
        "ffffffffffffffffffffffffffffffff",
        chargepilot_platform::jwt::Kind::User,
        1,
    )
    .unwrap();
    let (status, body) = get_with_bearer(router.clone(), "/api/v1/user/profile", &forged).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["message"], "access token invalid");
    // 畸形 Bearer
    let (status, _) = get_with_bearer(router, "/api/v1/user/profile", "garbage").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = account;
}
