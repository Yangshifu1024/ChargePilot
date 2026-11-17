#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! /admin SPA 深链回归：产物嵌入时深链回落 index.html 且
//! 状态 200；产物缺失（本地未构建前端）时静态路由不挂载、深链 404 可见即
//! 诊断。业务路由不触发（请求只打 /admin/*），连接池用 lazy 占位即可。

mod support;

use chargepilot_http_api::state::ApiDeps;
use std::sync::Arc;
use tower::util::ServiceExt as _;

#[tokio::test]
async fn deep_links_fall_back_to_index_with_200() {
    let pool = sqlx::MySqlPool::connect_lazy("mysql://invalid:0/none").unwrap();
    let deps = Arc::new(ApiDeps {
        db: pool.clone(),
        redis_url: Arc::new("redis://127.0.0.1:1".into()),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: Arc::new(chargepilot_platform::ids::SegmentAllocator::new(pool, 1)),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: None,
        wallet_recharge: None,
        checkout: None,
        simulation_payment: false,
        snapshots: None,
        charging_stop: None,
        export: chargepilot_mod_export::ExportConfig {
            export_dir: String::new(),
            max_rows: 0,
        },
    });
    let app = chargepilot_http_api::build_router(deps, support::prometheus_handle()).await;

    if chargepilot_assets_admin::is_empty() {
        // 本地无前端产物（嵌入空集合）：静态路由不挂载，深链 404 可见即诊断。
        for uri in ["/admin/", "/admin/login", "/admin/devices"] {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::NOT_FOUND,
                "{uri} 无产物时应 404（路由未挂载）"
            );
        }
        eprintln!("嵌入产物缺失：404 形态断言完成（构建前端后可验证 200 回退形态）");
        return;
    }
    for uri in ["/admin/", "/admin/login", "/admin/devices"] {
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(status, 200, "{uri} 深链应回落 index.html 且状态 200");
        assert!(
            String::from_utf8_lossy(&body).contains("<html"),
            "{uri} 回落正文应为 index.html"
        );
    }
}
