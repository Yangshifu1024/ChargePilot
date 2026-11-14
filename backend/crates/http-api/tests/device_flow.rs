//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 03 批 2 主缝集成测试：设备导入→台账、故障工单全流转、告警认领、C 端站点。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::json;
use support::{TestApp, flow_lock, rand_suffix, req};

async fn login_super(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> Option<(String, u64)> {
    let username = format!("p3b-{}", rand_suffix());
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
    body["data"]["access_token"]
        .as_str()
        .map(|s| (s.to_string(), id))
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn import_devices_and_fault_flow() {
    let _flow_lock = flow_lock();
    let app = TestApp::builder().build().await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let (token, _super_id) = login_super(&router, &db, &alloc).await.expect("超管登录");

    // 站点 + 厂商
    let (status, body) = req(&router, "POST", "/api/v1/admin/stations", Some(json!({
        "name": format!("导入站点{}", rand_suffix()), "longitude": 120.0, "latitude": 30.0, "status": "active"
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let station_id = body["data"]["id"].as_u64().unwrap();
    let vendor_code = format!("v{}", rand_suffix());
    let (status, body) = req(&router, "POST", "/api/v1/admin/vendors", Some(json!({
        "vendor_code": vendor_code, "vendor_name": "导入厂商", "adapter_class": "dc589", "protocol": "tcp", "status": "enabled"
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let vendor_id = body["data"]["id"].as_u64().unwrap();

    // 导入 2 台设备
    let import_id = format!("imp-{}", rand_suffix());
    let d1 = format!("DC{}", rand_suffix());
    let d2 = format!("DC{}", rand_suffix());
    let (status, body) = req(&router, "POST", "/api/v1/admin/device-imports", Some(json!({
        "import_id": import_id,
        "devices": [
            {"device_id": d1, "vendor_id": vendor_id, "station_id": station_id, "port_count": 2},
            {"device_id": d2, "vendor_id": vendor_id, "station_id": station_id, "port_count": 4}
        ]
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "导入应成功: {body}");
    assert_eq!(body["data"]["provisioned"], 2);

    // 台账核对：device_meta 2 行 + gateway device 2 行 + 端口 2+4
    let meta: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_meta` WHERE `station_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(station_id)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(meta, 2, "device_meta 应有 2 台");
    let gw_ports: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `device_port` p JOIN `device` d ON d.`device_id` = p.`device_id` WHERE d.`station_id` = ?",
    ).bind(station_id).fetch_one(db.as_ref()).await.unwrap();
    assert_eq!(gw_ports, 6, "gateway 端口应为 2+4=6");

    // 重放：已完成任务静默
    let (status, body) = req(&router, "POST", "/api/v1/admin/device-imports", Some(json!({
        "import_id": import_id,
        "devices": [
            {"device_id": d1, "vendor_id": vendor_id, "station_id": station_id, "port_count": 2},
            {"device_id": d2, "vendor_id": vendor_id, "station_id": station_id, "port_count": 4}
        ]
    })), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["provisioned"], 0, "重放不重复开通");

    // 前端「全部状态」以空串发送（axios 照发 status=）：空串须视同未传，返回全部。
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/admin/devices?status=&keyword=&page=1&page_size=50",
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
            .any(|d| d["device_id"].as_str() == Some(d1.as_str())),
        "空串过滤的列表应含已开通设备"
    );

    // 运行态合并契约（回归防护）：列表行带站点/厂商名、心跳/信号/端口与
    // runtime_available（device 行存在即可读），对齐旧版 Management.Enrich。
    let row = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"].as_str() == Some(d1.as_str()))
        .unwrap();
    assert_eq!(row["runtime_available"], true, "已登记设备应可读摘要");
    assert!(
        row["last_heartbeat_at"].is_null(),
        "未发心跳时心跳时刻为 null"
    );
    assert_eq!(
        row["ports"].as_array().map(|a| a.len()),
        Some(2),
        "端口运行态应带 port_count=2 的上报行"
    );
    let detail_id: u64 = sqlx::query_scalar("SELECT `id` FROM `device_meta` WHERE `device_id` = ?")
        .bind(&d1)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/admin/devices/{detail_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "详情应可读: {body}");
    assert!(
        body["data"]["station_name"].is_string(),
        "详情应带站点名: {body}"
    );
    assert!(
        body["data"]["warranty_until"].is_null(),
        "未设保修时为 null"
    );
    assert!(
        body["data"]["tags"].is_array() && body["data"]["updated_at"].is_string(),
        "详情 tags 为数组且带 updated_at 乐观锁值"
    );

    // 设备启停：disabled 后仅 enabled/disabled 可写
    let meta_id: u64 = sqlx::query_scalar("SELECT `id` FROM `device_meta` WHERE `device_id` = ?")
        .bind(&d1)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    let (status, _) = req(
        &router,
        "PUT",
        &format!("/api/v1/admin/devices/{meta_id}/status"),
        Some(json!({"status": "disabled"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 用户报修（登录 C 端）
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/auth/login",
        Some(json!({"code": "dev:fixer"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_token = body["data"]["jwt"].as_str().unwrap().to_string();
    let (status, body) = req(
        &router,
        "POST",
        "/api/v1/user/device/report-fault",
        Some(json!({
            "device_id": d1, "fault_type": "mechanical", "description": "插座松动"
        })),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "报修应成功: {body}");
    let report_id = body["data"]["id"].as_str().unwrap().to_string();
    let report_id_u64: u64 = report_id.parse().unwrap();

    // 后台派单 → 超管自己处理（有 fault.resolve 权限）
    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/device-fault-reports/{report_id_u64}/dispatch"),
        Some(json!({
            "assigned_to": _super_id, "note": "请尽快处理"
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "派单应成功: {body}");
    // fixed 无 note → 400
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/device-fault-reports/{report_id_u64}/resolve"),
        Some(json!({"status": "fixed"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "fixed 须带处理说明");
    // fixed → closed
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/device-fault-reports/{report_id_u64}/resolve"),
        Some(json!({"status": "fixed", "note": "已拧紧"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _body) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/device-fault-reports/{report_id_u64}/resolve"),
        Some(json!({"status": "closed", "note": "回访确认"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 回退 closed → fixed 拒绝
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/device-fault-reports/{report_id_u64}/resolve"),
        Some(json!({"status": "fixed", "note": "回退"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "回退应拒绝");

    // 告警：写入一条 active → 认领 → 重放静默
    sqlx::query(
        "INSERT INTO `alert_event` (`device_id`, `severity`, `metric`, `value`, `status`, `event_id`, `created_at`, `created_month`) \
         VALUES (?, 'fatal', 'smoke', 187, 'active', ?, NOW(3), DATE_FORMAT(NOW(3), '%Y-%m-01'))",
    )
    .bind(&d1).bind(format!("evt-{}", rand_suffix()))
    .execute(db.as_ref()).await.unwrap();
    let alert_id: u64 = sqlx::query_scalar(
        "SELECT `id` FROM `alert_event` WHERE `device_id` = ? ORDER BY `id` DESC LIMIT 1",
    )
    .bind(&d1)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/alerts/{alert_id}/ack"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/admin/alerts/{alert_id}/ack"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "认领重放静默");

    // C 端站点：nearby 与 detail
    let (status, body) = req(
        &router,
        "GET",
        "/api/v1/user/station/nearby?longitude=120.0&latitude=30.0&radius_km=50",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["located"], true);
    let found = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"].as_u64() == Some(station_id));
    assert!(found, "附近站点应包含新站点");
    let (status, body) = req(
        &router,
        "GET",
        &format!("/api/v1/user/station/{station_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"]["devices"].is_array(), "站点详情应含设备列表");

    // 测试尾部自清理（删除本测试建立的 p3b- 超管账号，防残留污染计数类断言）
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'p3b-%'")
        .execute(&*db)
        .await;
}
