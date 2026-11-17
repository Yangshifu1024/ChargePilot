#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 主缝集成测试：计费方案发布幂等/版本基线/继承、站点、厂商。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::json;
use support::{TestApp, flow_lock, rand_suffix, req};

async fn login_super(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> Option<String> {
    let suffix = rand_suffix();
    let username = format!("p3-{suffix}");
    let id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("super-pass-123").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(id).bind(&username).bind(hash).execute(db).await.unwrap();
    let (status, body) = req(
        router,
        "POST",
        "/api/v1/admin/auth/login",
        Some(json!({"username": username, "password": "super-pass-123"})),
        None,
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("登录失败: {body}");
        return None;
    }
    body["data"]["access_token"].as_str().map(str::to_string)
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn publish_inherit_and_version_baseline() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let token = login_super(&router, &db, &alloc).await.expect("超管登录");

    // 建站点 + 厂商
    let (status, body) = req(&router, "POST", "/api/v1/admin/stations", Some(json!({
        "name": format!("站点{}", rand_suffix()), "longitude": 120.1, "latitude": 30.2, "status": "active"
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "建站点应成功: {body}");
    let station_id = body["data"]["id"].as_u64().unwrap();
    // 前端「全部状态」以空串发送（axios 照发 status=）：空串须视同未传，返回全部。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/stations?status=&keyword=&page=1&page_size=50",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "空串状态应视同未传: {body}");
    assert!(
        body["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"].as_u64() == Some(station_id)),
        "空串过滤的列表应含新建站点"
    );
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/admin/vendors",
        Some(json!({
            "vendor_code": format!("v{}", rand_suffix()), "vendor_name": "测试厂商",
            "adapter_class": "dc589", "protocol": "tcp", "status": "enabled"
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建厂商应成功: {body}");

    // 建模板（有效方案）
    let scheme = json!({
        "name": format!("方案{}", rand_suffix()), "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [
            {"id": 1, "name": "1元", "mode": "amount", "price_cents": 100},
            {"id": 2, "name": "2小时", "mode": "duration", "price_cents": 200, "minutes": 120}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 2, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/admin/settings/charging-schemes",
        Some(json!({"scheme": scheme})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建模板应成功: {body}");
    let template_id = body["data"]["id"].as_u64().unwrap();

    // 发布到站点（版本基线 0）→ version=1
    let rid1 = format!("pub-{}", rand_suffix());
    let (status, body) = req(&router, "POST", "/api/v1/admin/settings/charging-schemes/apply", Some(json!({
        "request_id": rid1, "station_id": station_id, "template_id": template_id, "expected_version": 0
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "发布应成功: {body}");
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["replayed"], false);

    // 同 request_id 重放 → replayed=true
    let (status, body) = req(&router, "POST", "/api/v1/admin/settings/charging-schemes/apply", Some(json!({
        "request_id": rid1, "station_id": station_id, "template_id": template_id, "expected_version": 0
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["replayed"], true);

    // 同 request_id 不同内容 → 409
    let (status, _) = req(
        &router,
        "POST",
        "/api/v1/admin/settings/charging-schemes/apply",
        Some(json!({
            "request_id": rid1, "station_id": station_id, "scheme": scheme, "expected_version": 0
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "同 ID 不同内容应冲突");

    // 版本冲突：expected_version=0 再发（基线已是 1）→ 409
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/admin/settings/charging-schemes/apply",
        Some(json!({
            "request_id": format!("pub-{}", rand_suffix()), "station_id": station_id,
            "template_id": template_id, "expected_version": 0
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "版本基线冲突: {body}");

    // 生效查询：站点默认
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/stations/{station_id}/charging-scheme"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["inherited"], false);
    assert!(body["data"]["scheme"].is_object());

    // 设备覆盖发布（expected=1）→ version=2；生效查询设备优先
    let device_id = format!("dev-{}", rand_suffix());
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/admin/settings/charging-schemes/apply",
        Some(json!({
            "request_id": format!("pub-{}", rand_suffix()), "station_id": station_id,
            "device_id": device_id, "template_id": template_id, "expected_version": 0
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "设备覆盖发布应成功: {body}");
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/stations/{station_id}/charging-scheme?device_id={device_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["inherited"], false, "设备有自有规则不算继承");
    assert_eq!(body["data"]["version"], 1, "设备维度版本独立从 1 起");

    // 继承（删除设备覆盖）→ 设备回落站点默认
    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/stations/{station_id}/charging-scheme/inherit"),
        Some(json!({"device_id": device_id, "expected_version": 1})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "继承应成功: {body}");
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/stations/{station_id}/charging-scheme?device_id={device_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["inherited"], true, "删除覆盖后应走站点默认");
    assert_eq!(body["data"]["version"], 1, "回落到站点默认规则版本");

    // 计量匹配：设备优先 + 回落
    let rule =
        chargepilot_mod_pricing::metering::active_device_rule(&db, station_id, Some(&device_id))
            .await;
    assert!(rule.is_ok(), "生效规则应可查询");

    // 测试尾部自清理（删除本测试建立的 p3- 超管账号，防残留污染计数类断言）
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'p3-%'")
        .execute(&*db)
        .await;
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn invalid_scheme_is_rejected_with_conflict() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let token = login_super(&router, &db, &alloc).await.expect("超管登录");
    // 时段不覆盖全天 → 409
    let bad = json!({
        "name": "坏方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [{"electric_cents": 50, "service_cents": 30, "end_minute": 720}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {}, "card": {"package_id": 2, "max_minutes": 600}, "display": {"duration": [], "energy": []}
    });
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/admin/settings/charging-schemes",
        Some(json!({"scheme": bad})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "无效方案应 409: {body}");
    assert!(body["message"].as_str().unwrap().contains("全天"));

    // 测试尾部自清理
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'p3-%'")
        .execute(&*db)
        .await;
}
