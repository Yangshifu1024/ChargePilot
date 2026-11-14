//! HTTP 适配：axum 路由合并 + 鉴权中间件 + 统一信封输出（测试主缝所在层）。
//! 依赖方向：本层只调各域模块服务，不写业务 SQL、不持有业务表。

pub mod admin_finance;
pub mod admin_guard;
pub mod admin_orders;
pub mod admin_pricing_device;
pub mod admin_users_cards;
pub mod deps;
pub mod device_and_user;
pub mod errormap;
pub mod params;
pub mod user_auth;
pub mod user_guard;
pub mod user_identity_cards;
pub mod user_settlement;
pub mod views;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chargepilot_platform::error::{ApiError, Code};

/// 业务路由 + 健康检查 + 指标 + /admin 静态资源（SPA fallback）。
/// `admin_static_dir` 为 None 时（开发未构建前端）静态路由不挂载。
pub async fn build_router(
    deps: std::sync::Arc<deps::ApiDeps>,
    prometheus: metrics_exporter_prometheus::PrometheusHandle,
    admin_static_dir: Option<&str>,
    h5_static_dir: Option<&str>,
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
        .merge(admin_finance::router())
        .merge(views::order::admin::router())
        .merge(admin_orders::router())
        .merge(admin_pricing_device::router())
        .merge(views::pricing::admin::router())
        .merge(views::device::admin::router())
        .merge(admin_users_cards::router())
        .merge(device_and_user::router())
        .merge(views::marketing::user::router())
        .merge(views::ops::user::router())
        .merge(views::wallet::user::router())
        .merge(views::payment::user::simulation_router())
        .merge(views::charging::user::router())
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
    if let Some(h5_dir) = h5_static_dir {
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

// 票 17 收编：bearer/respond/identity_error 定义归 errormap（下方 re-export
// 保持既有 crate::respond / crate::bearer 引用路径不变）。
pub use errormap::{bearer, identity_error, respond};
