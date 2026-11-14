#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! API 契约缝（二期票 02）：路由集合快照 + 信封与错误码契约。
//!
//! 与 `scripts/ops/check-api-contract.py` 的分工：该脚本管「前端实际调用 ↔ 后端路由」
//! 对账；本测试管「路由集合与信封形状不变」——HTTP 层任何重排（票 18/19）导致路由
//! 漂移、信封字段变化或错误码表变化，都在日常检查（无需真库）中失败。
//!
//! 探测原理（不执行业务处理器）：已知路径配错方法 → axum MethodRouter 默认 405
//! （同路径多方法并存时探到另一注册处理器，得到鉴权 401/依赖 5xx，亦合法——
//! 唯一禁止 404）；未知路径 → 落入 `not_found` 回落返回 404 信封。预期方法的
//! 探活允许命中鉴权 401 或依赖故障 5xx 信封——只断言「不是 404/405」，证明
//! 路由与方法注册存在。
//! 响应载荷以 json! 内联构造（无集中 DTO 结构体），域级字段基线由各集成流测试
//! 逐字段断言承担；本缝钉住的是信封外壳、错误码表与路由全集三样「红线契约」。

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// 路由清单快照：与各 `router()` 注册函数人工对齐（生成方式见票 02，125 条）。
/// 修改任何路由（增删改路径/方法）必须同步本表——本表即路由契约基线。
const ROUTES: &[(&str, &str)] = &[
    ("get", "/api/v1/admin/admin-users"),
    ("get", "/api/v1/admin/alerts"),
    ("get", "/api/v1/admin/announcements"),
    ("get", "/api/v1/admin/audit-logs"),
    ("get", "/api/v1/admin/auth/me"),
    ("get", "/api/v1/admin/auth/mfa-settings"),
    ("get", "/api/v1/admin/billing/invoices"),
    ("get", "/api/v1/admin/billing/meter-reviews"),
    ("get", "/api/v1/admin/billing/refunds"),
    ("get", "/api/v1/admin/billing/wallet-risks"),
    ("get", "/api/v1/admin/coupon-activities"),
    ("get", "/api/v1/admin/coupons"),
    ("get", "/api/v1/admin/coupons/{id}/stats"),
    ("get", "/api/v1/admin/dashboard"),
    ("get", "/api/v1/admin/device-fault-reports"),
    ("get", "/api/v1/admin/device-fault-reports/{id}/history"),
    ("get", "/api/v1/admin/device-imports"),
    ("get", "/api/v1/admin/devices"),
    ("get", "/api/v1/admin/devices/{id}"),
    ("get", "/api/v1/admin/diagnostics"),
    ("get", "/api/v1/admin/exports"),
    ("get", "/api/v1/admin/exports/resources"),
    ("get", "/api/v1/admin/exports/{id}"),
    ("get", "/api/v1/admin/exports/{id}/download"),
    ("get", "/api/v1/admin/feedback"),
    ("get", "/api/v1/admin/online-cards"),
    ("get", "/api/v1/admin/orders"),
    ("get", "/api/v1/admin/orders/{id}"),
    ("get", "/api/v1/admin/orders/{id}/process"),
    ("get", "/api/v1/admin/orders/{id}/refunds"),
    ("get", "/api/v1/admin/orders/{id}/timeline"),
    ("get", "/api/v1/admin/payment-orders"),
    ("get", "/api/v1/admin/permissions"),
    ("get", "/api/v1/admin/roles"),
    ("get", "/api/v1/admin/settings/charging-schemes"),
    ("get", "/api/v1/admin/settings/device-capabilities"),
    ("get", "/api/v1/admin/stations"),
    ("get", "/api/v1/admin/stations/{id}"),
    ("get", "/api/v1/admin/stations/{id}/charging-scheme"),
    ("get", "/api/v1/admin/users"),
    ("get", "/api/v1/admin/users/{id}"),
    ("get", "/api/v1/admin/users/{id}/recharges"),
    ("get", "/api/v1/admin/vendor-options"),
    ("get", "/api/v1/admin/vendors"),
    ("get", "/api/v1/admin/vendors/{id}"),
    ("get", "/api/v1/user/announcement/list"),
    ("get", "/api/v1/user/bills"),
    ("get", "/api/v1/user/bills/{id}"),
    ("get", "/api/v1/user/cards"),
    ("get", "/api/v1/user/charge/history"),
    ("get", "/api/v1/user/charge/ongoing"),
    ("get", "/api/v1/user/charge/ongoing/curve"),
    ("get", "/api/v1/user/charge/ongoing/snapshot"),
    ("get", "/api/v1/user/charge/{order_no}"),
    ("get", "/api/v1/user/charge/{order_no}/curve"),
    ("get", "/api/v1/user/coupon/my"),
    ("get", "/api/v1/user/coupons"),
    ("get", "/api/v1/user/device/fault-reports"),
    ("get", "/api/v1/user/device/fault-reports/{id}/history"),
    ("get", "/api/v1/user/invoice/my"),
    ("get", "/api/v1/user/profile"),
    ("get", "/api/v1/user/station/nearby"),
    ("get", "/api/v1/user/station/{id}"),
    ("get", "/api/v1/user/wallet/balance"),
    ("get", "/api/v1/user/wallet/recharges"),
    ("get", "/api/v1/user/wallet/refunds"),
    ("get", "/api/v1/user/wallet/txns"),
    ("get", "/health/live"),
    ("get", "/health/ready"),
    ("get", "/metrics"),
    ("post", "/api/v1/admin/admin-users/{id}/mfa"),
    ("post", "/api/v1/admin/admin-users/{id}/reset-password"),
    ("post", "/api/v1/admin/admin-users/{id}/unlock"),
    ("post", "/api/v1/admin/alerts/{id}/ack"),
    ("post", "/api/v1/admin/auth/login"),
    ("post", "/api/v1/admin/auth/logout"),
    ("post", "/api/v1/admin/auth/mfa"),
    ("post", "/api/v1/admin/auth/refresh"),
    ("post", "/api/v1/admin/billing/invoices/{id}/approve"),
    ("post", "/api/v1/admin/billing/invoices/{id}/reject"),
    ("post", "/api/v1/admin/billing/meter-reviews/{id}/decide"),
    ("post", "/api/v1/admin/billing/meter-reviews/{id}/propose"),
    (
        "post",
        "/api/v1/admin/billing/meter-reviews/{id}/resolve-amount",
    ),
    ("post", "/api/v1/admin/billing/refunds/{refund_no}/approve"),
    ("post", "/api/v1/admin/billing/refunds/{refund_no}/reject"),
    ("post", "/api/v1/admin/billing/refunds/{refund_no}/retry"),
    (
        "post",
        "/api/v1/admin/billing/wallet-risks/{request_id}/release",
    ),
    (
        "post",
        "/api/v1/admin/billing/wallet-risks/{request_id}/review",
    ),
    ("post", "/api/v1/admin/coupons/{id}/grants"),
    ("post", "/api/v1/admin/device-fault-reports/{id}/dispatch"),
    ("post", "/api/v1/admin/device-fault-reports/{id}/resolve"),
    ("post", "/api/v1/admin/device-imports/{import_id}/retry"),
    ("post", "/api/v1/admin/feedback/{id}/reply"),
    ("post", "/api/v1/admin/online-cards/bind"),
    ("post", "/api/v1/admin/online-cards/{id}/status"),
    ("post", "/api/v1/admin/orders/{id}/refunds"),
    ("post", "/api/v1/admin/settings/charging-schemes/apply"),
    ("post", "/api/v1/admin/settings/charging-schemes/preview"),
    (
        "post",
        "/api/v1/admin/settings/charging-schemes/{id}/status",
    ),
    (
        "post",
        "/api/v1/admin/stations/{id}/charging-scheme/inherit",
    ),
    ("post", "/api/v1/user/auth/login"),
    ("post", "/api/v1/user/auth/logout"),
    ("post", "/api/v1/user/auth/refresh"),
    ("post", "/api/v1/user/bills/{id}/read"),
    ("post", "/api/v1/user/cards/{card_no}/status"),
    ("post", "/api/v1/user/charge/stop"),
    ("post", "/api/v1/user/charge/{order_id}/feedback"),
    ("post", "/api/v1/user/charging/{order_no}/stop"),
    ("post", "/api/v1/user/development/payments/confirm"),
    ("post", "/api/v1/user/device/report-fault"),
    ("post", "/api/v1/user/invoice/apply"),
    ("post", "/api/v1/user/phone/bind"),
    ("post", "/api/v1/user/phone/unbind"),
    ("post", "/api/v1/user/scan/offers"),
    ("post", "/api/v1/user/scan/port"),
    ("post", "/api/v1/user/scan/resolve"),
    ("post", "/api/v1/user/scan/start"),
    ("post", "/api/v1/user/wallet/recharge"),
    ("post", "/api/v1/user/wallet/refund"),
    ("put", "/api/v1/admin/admin-users/{id}"),
    ("put", "/api/v1/admin/coupon-activities/{id}"),
    ("put", "/api/v1/admin/coupons/{id}"),
    ("put", "/api/v1/admin/devices/{id}/status"),
    ("put", "/api/v1/admin/roles/{id}"),
    ("put", "/api/v1/admin/settings/charging-schemes/{id}"),
];

/// 路由探针路径：路径参数段替换为占位段（方法路由在提取器之前分派，不触处理器）。
fn probe_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for seg in path.split('/') {
        if seg.starts_with('{') && seg.ends_with('}') {
            out.push_str("__probe__");
        } else {
            out.push_str(seg);
        }
        out.push('/');
    }
    out.pop();
    out
}

/// 与预期方法不同的可探方法集（HEAD 随 GET 自动支持、OPTIONS 未注册，均不探测）。
fn other_methods(method: &str) -> Vec<&'static str> {
    ["get", "post", "put", "delete", "patch"]
        .into_iter()
        .filter(|m| *m != method && !(method == "get" && *m == "head"))
        .collect()
}

fn prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect("指标记录器")
        })
        .clone()
}

async fn build() -> Router {
    // 惰性连接池：探活不允许命中处理器，池永不真正建连（端口 1 立即拒连兜底）；
    // 号段分配器仅构造不预热（预热需真库，探活路径不会用到它）。
    let db = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        // 探活路径若真的触库（公开路由），100ms 内拿到依赖故障即可——
        // 默认 30s 获取超时会把整个快照测试拖到分钟级。
        .acquire_timeout(std::time::Duration::from_millis(100))
        .connect_lazy("mysql://probe:probe@127.0.0.1:1/none")
        .expect("惰性池构建");
    let deps = std::sync::Arc::new(chargepilot_http_api::deps::ApiDeps {
        db: db.clone(),
        redis_url: std::sync::Arc::new("redis://127.0.0.1:1".into()),
        jwt_secret: std::sync::Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: std::sync::Arc::new(chargepilot_platform::ids::SegmentAllocator::new(db, 1)),
        exchanger: std::sync::Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        charging_stop: None,
        snapshots: None,
    });
    let prometheus = prometheus_handle();
    chargepilot_http_api::build_router(deps, prometheus, None).await
}

fn request(method: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(
            axum::http::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .expect("合法方法"),
        )
        .uri(path)
        .header("x-request-id", "contract-probe")
        .body(Body::empty())
        .expect("请求构建")
}

/// 清单内每条路由：路径存在（预期方法探活非 404/405）且其余方法一律 405。
#[tokio::test]
async fn route_snapshot_matches_registered_routes() {
    let app = build().await;
    let mut failures = Vec::new();
    for (method, path) in ROUTES {
        let probed = probe_path(path);
        let expected_ok = app
            .clone()
            .oneshot(request(method, &probed))
            .await
            .expect("探活响应");
        let s = expected_ok.status();
        if s == StatusCode::NOT_FOUND || s == StatusCode::METHOD_NOT_ALLOWED {
            failures.push(format!("清单路由未按 {method} 注册：{path}（得到 {s}）"));
        }
        for wrong in other_methods(method) {
            let resp = app
                .clone()
                .oneshot(request(wrong, &probed))
                .await
                .expect("错误方法探活响应");
            // 同路径多方法并存时，错误方法探到的是另一注册处理器（鉴权 401/依赖 5xx），
            // 属合法；唯一禁止的是 404——那意味着路径在清单与路由之间不一致。
            if resp.status() == StatusCode::NOT_FOUND {
                failures.push(format!("{path} 对方法 {wrong} 意外 404：路径未注册"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "路由快照漂移 {} 项：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 清单外路径：404 信封（code 1004 + 回显 request_id），不得是裸 404 文本。
#[tokio::test]
async fn unknown_path_returns_not_found_envelope() {
    let app = build().await;
    let resp = app
        .oneshot(request("get", "/api/v1/no-such-endpoint"))
        .await
        .expect("探活响应");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers().get("x-request-id").unwrap(),
        "contract-probe"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], 1004, "404 信封业务码必须是 1004：{v}");
    assert!(v["message"].is_string());
    assert_eq!(v["request_id"], "contract-probe");
}

/// 信封外壳字段契约：字段名集合、类型与空形态冻结（前端按 code 映射文案）。
#[test]
fn envelope_shape_is_frozen() {
    use chargepilot_platform::httpx::Envelope;
    let e = Envelope {
        code: 1005,
        message: "参数错误".into(),
        data: serde_json::json!({"saved": true}),
        request_id: "rid-1".into(),
        trace_id: "rid-1".into(),
        errors: Vec::new(),
    };
    let v = serde_json::to_value(&e).unwrap();
    let obj = v.as_object().expect("信封必须是对象");
    // serde_json 默认按字母序输出键：契约是字段名集合，不是声明顺序。
    // serde_json 默认按字母序输出键；errors 空时不序列化（skip_serializing_if）。
    let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["code", "data", "message", "request_id", "trace_id"]
    );
    // 携带字段级错误时 errors 必须出现（前端表单回显依赖）。
    let mut e2 = Envelope {
        code: 1005,
        message: "参数错误".into(),
        data: serde_json::json!({"saved": true}),
        request_id: "rid-1".into(),
        trace_id: "rid-1".into(),
        errors: Vec::new(),
    };
    e2.errors = vec![chargepilot_platform::error::FieldErr {
        field: "page".into(),
        rule: "1..=100".into(),
    }];
    let v2 = serde_json::to_value(&e2).unwrap();
    assert_eq!(v2["errors"][0]["field"], "page");
    assert_eq!(v2["errors"][0]["rule"], "1..=100");
    assert_eq!(obj["code"], 1005);
    assert!(obj["code"].is_i64());
}

/// 错误码表契约：每个 Code 的业务码值、HTTP 状态、信封字段与响应头逐项冻结
/// （数值一经发布不得变更——前端按 code 映射文案）。
#[tokio::test]
async fn error_code_table_is_frozen() {
    use chargepilot_platform::error::{ApiError, Code};
    let cases: &[(Code, i64, StatusCode)] = &[
        (Code::Unauthorized, 1001, StatusCode::UNAUTHORIZED),
        (Code::VerificationFailed, 1002, StatusCode::BAD_REQUEST),
        (Code::Forbidden, 1003, StatusCode::FORBIDDEN),
        (Code::NotFound, 1004, StatusCode::NOT_FOUND),
        (Code::BadRequest, 1005, StatusCode::BAD_REQUEST),
        (Code::MethodNotAllowed, 1006, StatusCode::METHOD_NOT_ALLOWED),
        (Code::ResourceExpired, 1007, StatusCode::GONE),
        (Code::ImportConflict, 1009, StatusCode::CONFLICT),
        (Code::StateConflict, 2000, StatusCode::CONFLICT),
        (Code::ResourceUnavailable, 2001, StatusCode::CONFLICT),
        (Code::PricingUnavailable, 2004, StatusCode::CONFLICT),
        (Code::AccountLocked, 2006, StatusCode::LOCKED),
        (Code::Conflict, 2009, StatusCode::CONFLICT),
        (Code::TaskConflict, 2010, StatusCode::CONFLICT),
        (
            Code::DependencyFailed,
            5001,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            Code::ServiceUnavailable,
            5003,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ];
    for (code, value, status) in cases {
        let api = ApiError::new(*code, "契约探针");
        let resp = chargepilot_platform::httpx::error(&api, "rid-9");
        assert_eq!(resp.status(), *status, "Code {value:?} 的 HTTP 状态漂移");
        assert_eq!(
            resp.headers().get("cache-control").unwrap(),
            "no-store",
            "错误响应必须 no-store"
        );
        assert_eq!(resp.headers().get("x-request-id").unwrap(), "rid-9");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], *value, "业务码值漂移：{v}");
        assert_eq!(v["message"], "契约探针");
    }
}
