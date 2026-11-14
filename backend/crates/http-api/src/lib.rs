//! HTTP 适配：axum 路由合并 + 鉴权中间件 + 统一信封输出（测试主缝所在层）。
//! 依赖方向：本层只调各域模块服务，不写业务 SQL、不持有业务表。

pub mod errormap;
pub mod guard;
pub mod params;
pub mod state;
pub mod user_guard;
pub mod views;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chargepilot_platform::error::{ApiError, Code};

/// 业务路由 + 健康检查 + 指标 + /admin 静态资源（SPA fallback，票 22 嵌入）。
/// 前端产物经编译期嵌入（assets-admin / assets-h5 叶子 crate）；产物缺失
/// （本地未构建前端）时嵌入为空集合，静态路由不挂载。
pub async fn build_router(
    deps: std::sync::Arc<state::ApiDeps>,
    prometheus: metrics_exporter_prometheus::PrometheusHandle,
) -> axum::Router {
    let mut app = axum::Router::new()
        // 域路由注册绑定装配开关（票 17）：开关开启后该域路由才存在，
        // 域处理器对服务的取用必达（ChargingStopState/PaymentState 等）。
        .merge(if deps.wallet.is_some() {
            views::wallet::user::refund_router()
        } else {
            axum::Router::new()
        })
        .merge(if deps.checkout.is_some() {
            views::payment::user::checkout_router()
        } else {
            axum::Router::new()
        })
        .merge(if deps.charging_stop.is_some() {
            views::charging::user::stop_router()
        } else {
            axum::Router::new()
        })
        .merge(views::ops::auth::auth_router())
        .merge(views::ops::admin::management_router())
        .merge(views::ops::diagnostics::diagnostics_router())
        .merge(views::ops::audit::router())
        .merge(views::export::admin::router())
        .merge(views::marketing::admin::router())
        .merge(views::wallet::admin::router())
        .merge(views::settlement::admin::router())
        .merge(views::order::admin::router())
        .merge(views::order::admin::admin_router())
        .merge(views::pricing::admin::router())
        .merge(views::pricing::admin::scheme_router())
        .merge(views::device::admin::router())
        .merge(views::device::admin::station_router())
        .merge(views::device::admin::device_admin_router())
        .merge(views::identity::admin::router())
        .merge(views::card::admin::router())
        .merge(views::marketing::user::router())
        .merge(views::ops::user::router())
        .merge(views::wallet::user::router())
        .merge(views::payment::user::simulation_router())
        .merge(views::charging::user::router())
        .merge(views::settlement::user::router())
        .merge(views::identity::user::router())
        .merge(views::card::user::router())
        .merge(views::order::user::router())
        .merge(views::device::user::router())
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
    // 票 22：编译期嵌入静态产物（磁盘挂载与 ADMIN_STATIC_DIR/H5_STATIC_DIR
    // 环境变量退役）。产物缺失时嵌入为空集合，路由不挂载（404 可见即诊断）。
    if !chargepilot_assets_admin::is_empty() {
        app = app
            // 根路由显式登记：axum 0.8 通配 {*rest} 不匹配空尾，/admin 与
            // /admin/ 是浏览器直接访问的入口，不能落到 fallback 404。
            .route("/admin", axum::routing::get(admin_root))
            .route("/admin/", axum::routing::get(admin_root))
            .route("/admin/{*rest}", axum::routing::get(admin_asset));
    }
    if !chargepilot_assets_h5::is_empty() {
        app = app
            .route("/h5", axum::routing::get(h5_root))
            .route("/h5/", axum::routing::get(h5_root))
            .route("/h5/{*rest}", axum::routing::get(h5_asset));
    }
    app
}

/// MIME 猜测（按扩展名；嵌入资源带完整内容类型响应）。
fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn embedded_response(
    get: impl Fn(&str) -> Option<Vec<u8>>,
    index: impl Fn() -> Option<Vec<u8>>,
    rest: Option<String>,
    root_index: bool,
) -> Response {
    let path = match (&rest, root_index) {
        (None, true) | (Some(_), true) if rest.as_deref().unwrap_or("").is_empty() => {
            "index.html".to_string()
        }
        (Some(r), _) => r.clone(),
        (None, _) => String::new(),
    };
    if let Some(bytes) = get(&path) {
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime_of(&path))],
            bytes,
        )
            .into_response();
    }
    // SPA 回退：未命中路径回落 index.html（history 路由「刷新不 404」）。
    if let Some(bytes) = index() {
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            bytes,
        )
            .into_response();
    }
    Response::builder()
        .status(axum::http::StatusCode::NOT_FOUND)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| axum::http::StatusCode::NOT_FOUND.into_response())
}

/// 管理端 SPA 根：/admin 与 /admin/（wildcard 不匹配空尾，根入口显式登记）。
async fn admin_root() -> Response {
    embedded_response(
        chargepilot_assets_admin::get,
        chargepilot_assets_admin::index,
        None,
        true,
    )
    .await
}

/// 管理端 SPA：/admin/{*rest}（深链回落 index）。
async fn admin_asset(rest: Option<axum::extract::Path<String>>) -> Response {
    let rest = rest.map(|p| p.0);
    embedded_response(
        chargepilot_assets_admin::get,
        chargepilot_assets_admin::index,
        rest,
        true,
    )
    .await
}

/// 用户端 H5 根：/h5 与 /h5/（根入口显式登记）。
async fn h5_root() -> Response {
    embedded_response(
        chargepilot_assets_h5::get,
        chargepilot_assets_h5::index,
        None,
        true,
    )
    .await
}

/// 用户端 H5：/h5/{*rest}（hash 路由，资源直出，未命中 404）。
async fn h5_asset(rest: Option<axum::extract::Path<String>>) -> Response {
    let rest = rest.map(|p| p.0);
    embedded_response(
        chargepilot_assets_h5::get,
        chargepilot_assets_h5::index,
        rest,
        true,
    )
    .await
}

async fn readiness(State(state): State<std::sync::Arc<state::ApiDeps>>) -> &'static str {
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

// 票 17 收编：bearer/respond/identity_error 定义归 errormap（下方 re-export
// 保持既有 crate::respond / crate::bearer 引用路径不变）。
pub use errormap::{bearer, identity_error, respond};
