//! HTTP 适配：axum 路由合并 + 鉴权中间件 + 统一信封输出（测试主缝所在层）。
//! 依赖方向：本层只调各域模块服务，不写业务 SQL、不持有业务表。

pub mod admin_auth;
pub mod admin_diagnostics;
pub mod admin_export;
pub mod admin_finance;
pub mod admin_guard;
pub mod admin_management;
pub mod admin_marketing;
pub mod admin_misc;
pub mod admin_orders;
pub mod admin_pricing_device;
pub mod admin_users_cards;
pub mod deps;
pub mod device_and_user;
pub mod user_auth;
pub mod user_charging;
pub mod user_identity_cards;
pub mod user_marketing;
pub mod user_ops;
pub mod user_payment;
pub mod user_settlement;
pub mod user_wallet;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chargepilot_platform::error::{ApiError, Code};

/// 业务路由 + 健康检查 + 指标 + /admin 静态资源（SPA fallback）。
/// `admin_static_dir` 为 None 时（开发未构建前端）静态路由不挂载。
pub async fn build_router(
    deps: std::sync::Arc<deps::ApiDeps>,
    prometheus: metrics_exporter_prometheus::PrometheusHandle,
    admin_static_dir: Option<&str>,
) -> axum::Router {
    let mut app = axum::Router::new()
        .merge(admin_auth::router())
        .merge(admin_management::router())
        .merge(admin_diagnostics::router())
        .merge(admin_export::router())
        .merge(admin_marketing::router())
        .merge(admin_finance::router())
        .merge(admin_misc::router())
        .merge(admin_orders::router())
        .merge(admin_pricing_device::router())
        .merge(admin_users_cards::router())
        .merge(device_and_user::router())
        .merge(user_marketing::router())
        .merge(user_ops::router())
        .merge(user_wallet::router())
        .merge(user_payment::router())
        .merge(user_charging::router())
        .merge(user_settlement::router())
        .merge(user_identity_cards::router())
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
        .with_state(deps);
    if let Some(dir) = admin_static_dir {
        // SPA：资源存在则直出，其余 /admin/* 回落 index.html（history 路由）。
        // ServeDir 的 not_found_service 会把 fallback 输出强制为 404（tower-http
        // 语义），刷新深链拿 404 不符合「刷新不 404」——外包一层：404 时改答
        // index（启动读入内存，产物与镜像同生命周期）。
        match tokio::fs::read(format!("{dir}/index.html")).await {
            Ok(bytes) => {
                app = app.nest_service(
                    "/admin",
                    SpaFallback {
                        inner: tower_http::services::ServeDir::new(dir),
                        index: std::sync::Arc::new(bytes),
                    },
                );
            }
            Err(_) => {
                // index 缺失属部署错误：只挂资源直出，深链 404 可见即可诊断。
                tracing::warn!(dir, "admin index.html 缺失，SPA 深链回落未启用");
                app = app.nest_service("/admin", tower_http::services::ServeDir::new(dir));
            }
        }
    }
    // H5 静态产物：hash 路由，资源直出即可（/h5/ 入口由 index.html 承载）。
    if let Ok(h5_dir) = std::env::var("H5_STATIC_DIR")
        && !h5_dir.is_empty()
    {
        app = app.nest_service("/h5", tower_http::services::ServeDir::new(h5_dir));
    }
    app
}

/// SPA 服务：内层 ServeDir 直出静态资源；未命中（404）时改答 index.html（200）。
/// index 在启动时读入内存（产物与镜像同生命周期，无需每次读盘）。
#[derive(Clone)]
struct SpaFallback {
    inner: tower_http::services::ServeDir,
    index: std::sync::Arc<Vec<u8>>,
}

type SpaFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<axum::response::Response, std::convert::Infallible>>
            + Send,
    >,
>;

impl tower::Service<axum::extract::Request> for SpaFallback {
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = SpaFuture;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        tower::Service::<axum::extract::Request>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        let inner = self.inner.clone();
        let index = self.index.clone();
        Box::pin(async move {
            let resp = tower::util::ServiceExt::oneshot(inner, req).await?;
            if resp.status() == axum::http::StatusCode::NOT_FOUND {
                Ok((
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    (*index).clone(),
                )
                    .into_response())
            } else {
                // ServeDir 响应体（tower-http 专用 body）归一为 axum body。
                Ok(resp.map(axum::body::Body::new))
            }
        })
    }
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
        axum::http::StatusCode::from_u16(status)
            .unwrap_or_else(|_| panic!("契约内状态码非法: {status}")),
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
