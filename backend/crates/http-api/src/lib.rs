//! HTTP 适配：axum 路由合并 + 鉴权中间件 + 统一信封输出（测试主缝所在层）。
//! 依赖方向：本层只调各域模块服务，不写业务 SQL、不持有业务表。

pub mod admin_auth;
pub mod admin_guard;
pub mod admin_management;
pub mod deps;
pub mod user_auth;
pub mod user_ops;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chargepilot_platform::error::{ApiError, Code};

/// 业务路由 + 健康检查 + 指标（app 的 serve 只负责监听与停机）。
pub fn build_router(
    deps: std::sync::Arc<deps::ApiDeps>,
    prometheus: metrics_exporter_prometheus::PrometheusHandle,
) -> axum::Router {
    axum::Router::new()
        .merge(admin_auth::router())
        .merge(admin_management::router())
        .merge(user_ops::router())
        .merge(user_auth::router())
        .route("/health/live", axum::routing::get(|| async { "ok" }))
        .route("/health/ready", axum::routing::get(readiness))
        .route(
            "/metrics",
            axum::routing::get(move || {
                let handle = prometheus.clone();
                async move { handle.render() }
            }),
        )
        .fallback(not_found)
        .with_state(deps)
}

async fn readiness(State(state): State<std::sync::Arc<deps::ApiDeps>>) -> &'static str {
    let db_ok = sqlx::query("SELECT 1").execute(&state.db).await.is_ok();
    let redis_ok = match redis::Client::open(state.redis_url.as_str()) {
        Ok(client) => match client.get_multiplexed_tokio_connection().await {
            Ok(mut conn) => redis::cmd("PING")
                .query_async::<()>(&mut conn)
                .await
                .is_ok(),
            Err(_) => false,
        },
        Err(_) => false,
    };
    if db_ok && redis_ok { "ok" } else { "degraded" }
}

async fn not_found(headers: axum::http::HeaderMap) -> Response {
    let rid = chargepilot_platform::httpx::request_id_from(&headers);
    let api = ApiError::new(Code::NotFound, "接口尚未提供，请确认服务版本");
    chargepilot_platform::httpx::error(&api, &rid)
}

/// 从请求头解析 Bearer 凭证（<scheme> <token>）。
pub fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// identity 域错误 → HTTP 响应（码值与文案沿用旧版字面量契约）。
pub fn identity_error(err: chargepilot_mod_identity::IdentityError, request_id: &str) -> Response {
    use chargepilot_mod_identity::IdentityError as E;
    match &err {
        E::InvalidRefresh => respond(401, 1001, "refresh token invalid", request_id),
        E::TokenInvalid => respond(401, 1001, "access token invalid", request_id),
        E::SessionExpired => respond(401, 1001, "session expired", request_id),
        E::AccountFrozen => respond(403, 1003, "account frozen", request_id),
        E::ExchangeFailed(_) => respond(502, 3001, "WeChat login failed", request_id),
        E::Storage(_) => {
            let api = ApiError::from(anyhow::anyhow!(err.to_string()));
            chargepilot_platform::httpx::error(&api, request_id)
        }
    }
}

/// 直发指定状态/码/文案的信封（3001 等旧版字面量码不在 Code enum 内，契约优先）。
pub fn respond(status: u16, code: i64, message: &str, request_id: &str) -> Response {
    let body = chargepilot_platform::httpx::Envelope {
        code,
        message: message.to_string(),
        data: serde_json::Value::Null,
        request_id: request_id.to_string(),
        trace_id: request_id.to_string(),
        errors: Vec::new(),
    };
    let _ = Code::BadRequest; // 保持错误码模块可见
    let mut resp = (
        axum::http::StatusCode::from_u16(status).unwrap(),
        axum::Json(body),
    )
        .into_response();
    resp.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}
