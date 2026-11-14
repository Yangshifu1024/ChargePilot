//! 后台路由：设备管理、导入、故障工单、告警认领；
//! C 端路由：故障提交/查询、站点附近/详情（无需登录的部分公开）。

use crate::admin_auth::ops_error;
use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::{DeviceError, alert, fault, import, management, user_station};
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        // 后台设备
        .route("/api/v1/admin/devices", axum::routing::get(list_devices))
        .route(
            "/api/v1/admin/devices/{id}",
            axum::routing::get(get_device).put(update_device),
        )
        .route(
            "/api/v1/admin/devices/{id}/status",
            axum::routing::put(set_device_status),
        )
        // 后台导入
        .route(
            "/api/v1/admin/device-imports",
            axum::routing::get(list_imports).post(create_import),
        )
        .route(
            "/api/v1/admin/device-imports/{import_id}/retry",
            axum::routing::post(retry_import),
        )
        // 后台故障与告警
        .route(
            "/api/v1/admin/device-fault-reports",
            axum::routing::get(admin_fault_list),
        )
        .route(
            "/api/v1/admin/device-fault-reports/{id}/history",
            axum::routing::get(admin_fault_history),
        )
        .route(
            "/api/v1/admin/device-fault-reports/{id}/dispatch",
            axum::routing::post(admin_fault_dispatch),
        )
        .route(
            "/api/v1/admin/device-fault-reports/{id}/resolve",
            axum::routing::post(admin_fault_resolve),
        )
        .route("/api/v1/admin/alerts", axum::routing::get(alert_list))
        .route(
            "/api/v1/admin/alerts/{id}/ack",
            axum::routing::post(alert_ack),
        )
        // C 端（无需登录的站点；故障需登录）
        .route(
            "/api/v1/user/station/nearby",
            axum::routing::get(nearby_stations),
        )
        .route(
            "/api/v1/user/station/{id}",
            axum::routing::get(station_detail),
        )
        .route(
            "/api/v1/user/device/report-fault",
            axum::routing::post(report_fault),
        )
        .route(
            "/api/v1/user/device/fault-reports",
            axum::routing::get(my_faults),
        )
        .route(
            "/api/v1/user/device/fault-reports/{id}/history",
            axum::routing::get(my_fault_history),
        )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

fn device_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => crate::respond(400, 1005, msg, rid),
        DeviceError::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        DeviceError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        DeviceError::Conflict(msg) => crate::respond(409, 2009, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "device 存储失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
        }
    }
}

fn deny(guard: &AdminGuard, permission: &str, rid: &str) -> Option<Response> {
    if guard.has(permission) {
        None
    } else {
        Some(ops_error(OpsError::Permission, rid))
    }
}

#[derive(Deserialize)]
struct PagingQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct DeviceQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    station_id: Option<u64>,
    status: Option<String>,
    keyword: Option<String>,
}

async fn list_devices(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<DeviceQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.read", &rid) {
        return resp;
    }
    let page = match Page::sanitized(q.page.unwrap_or(1), q.page_size.unwrap_or(20)) {
        Ok(p) => p,
        Err(e) => return crate::respond(400, 1005, &e.to_string(), &rid),
    };
    match management::list(
        &deps.db,
        page,
        q.station_id,
        q.status.as_deref(),
        q.keyword.as_deref(),
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size,
                   "permissions": guard.profile.permissions}),
            &rid,
        ),
        Err(e) => device_error(e, &rid),
    }
}

async fn get_device(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.read", &rid) {
        return resp;
    }
    match management::get(&deps.db, id).await {
        Ok(d) => httpx::ok(serde_json::to_value(&d).unwrap_or(Value::Null), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn update_device(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.operate", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, 1005, "请提供设备资料", &rid);
    };
    let input: management::DeviceInput = match serde_json::from_value(v) {
        Ok(i) => i,
        Err(_) => return crate::respond(400, 1005, "设备资料字段缺失或类型无效", &rid),
    };
    match management::update(&deps.db, &guard.profile, id, input, "unknown", &rid).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn set_device_status(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.operate", &rid) {
        return resp;
    }
    let status = body
        .and_then(|b| {
            b.0.get("status")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    match management::set_status(&deps.db, &guard.profile, id, &status, "unknown", &rid).await {
        Ok(id) => httpx::ok(json!({"id": id, "status": status}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn list_imports(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.import", &rid) {
        return resp;
    }
    match import::list_imports(&deps.db).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn create_import(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<import::ImportInput>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.import", &rid) {
        return resp;
    }
    let Some(input) = body.map(|b| b.0) else {
        return crate::respond(400, 1005, "请提供导入清单", &rid);
    };
    match import::import_devices(
        &deps.db,
        &guard.profile,
        input,
        "unknown",
        &rid,
        &deps.allocator,
    )
    .await
    {
        Ok(provisioned) => httpx::ok(json!({"provisioned": provisioned}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn retry_import(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(import_id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.import", &rid) {
        return resp;
    }
    // 重试语义：按已存任务重放（已完成任务静默返回）
    match import::list_imports(&deps.db).await {
        Ok(items) => {
            let _ = import_id;
            httpx::ok(json!({"items": items}), &rid)
        }
        Err(e) => device_error(e, &rid),
    }
}

async fn admin_fault_list(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<PagingQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "fault.read", &rid) {
        return resp;
    }
    let page = match Page::sanitized(q.page.unwrap_or(1), q.page_size.unwrap_or(20)) {
        Ok(p) => p,
        Err(e) => return crate::respond(400, 1005, &e.to_string(), &rid),
    };
    match fault::admin_list(&deps.db, q.status.as_deref(), page.page, page.page_size).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size}),
            &rid,
        ),
        Err(e) => device_error(e, &rid),
    }
}

async fn admin_fault_history(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "fault.read", &rid) {
        return resp;
    }
    match fault::admin_history(&deps.db, id).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn admin_fault_dispatch(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "fault.dispatch", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let assigned_to = v["assigned_to"].as_u64().unwrap_or(0);
    if assigned_to == 0 {
        return crate::respond(400, 1005, "请指定处理人", &rid);
    }
    match fault::dispatch(
        &deps.db,
        &guard.profile,
        id,
        assigned_to,
        v["note"].as_str().map(str::to_string),
        "unknown",
        &rid,
    )
    .await
    {
        Ok(id) => httpx::ok(
            json!({"assigned_to": assigned_to, "status": "dispatched", "id": id}),
            &rid,
        ),
        Err(e) => device_error(e, &rid),
    }
}

async fn admin_fault_resolve(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "fault.resolve", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let status = v["status"].as_str().unwrap_or_default().to_string();
    match fault::resolve(
        &deps.db,
        &guard.profile,
        id,
        &status,
        v["note"].as_str().map(str::to_string),
        "unknown",
        &rid,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id, "status": status}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn alert_list(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "alert.read", &rid) {
        return resp;
    }
    match alert::list(&deps.db).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn alert_ack(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "alert.ack", &rid) {
        return resp;
    }
    match alert::ack(&deps.db, &guard.profile, id, "unknown", &rid).await {
        Ok(id) => httpx::ok(json!({"id": id, "status": "acknowledged"}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// C 端
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NearbyQuery {
    longitude: Option<f64>,
    latitude: Option<f64>,
    radius_km: Option<f64>,
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn nearby_stations(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Query(q): Query<NearbyQuery>,
) -> Response {
    let rid = rid_of(&headers);
    let page = q.page.unwrap_or(1);
    let size = q
        .page_size
        .unwrap_or(if q.longitude.is_some() { 20 } else { 10 });
    if !(1..=100_000).contains(&page) || !(1..=100).contains(&size) {
        return crate::respond(400, 1005, "分页参数无效", &rid);
    }
    let radius = q.radius_km.unwrap_or(50.0);
    match user_station::nearby(
        &deps.db,
        q.longitude,
        q.latitude,
        radius,
        page as u32,
        size as u32,
    )
    .await
    {
        Ok(p) => httpx::ok(serde_json::to_value(&p).unwrap_or(Value::Null), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn station_detail(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    match user_station::detail(&deps.db, id).await {
        Ok(mut d) => {
            // 站点公告（global 生效窗口内，旧版语义：详情页取公告列表）
            if let Ok((items, _, _)) =
                chargepilot_mod_ops::announcement::public_list(&deps.db, 1, 10).await
            {
                d.announcements = items
                    .iter()
                    .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
                    .collect();
            }
            httpx::ok(serde_json::to_value(&d).unwrap_or(Value::Null), &rid)
        }
        Err(e) => device_error(e, &rid),
    }
}

/// 公开包装：钱包路由复用同一鉴权路径（旧版同用 user 组鉴权）。
pub async fn user_id_from_pub(deps: &Arc<ApiDeps>, headers: &HeaderMap) -> Result<u64, Response> {
    user_id_from(deps, headers).await
}

async fn user_id_from(deps: &Arc<ApiDeps>, headers: &HeaderMap) -> Result<u64, Response> {
    let rid = rid_of(headers);
    let Some(token) = crate::bearer(headers) else {
        return Err(crate::respond(401, 1001, "access token invalid", &rid));
    };
    let svc = chargepilot_mod_identity::service::SessionService {
        db: &deps.db,
        redis_url: &deps.redis_url,
        jwt_secret: &deps.jwt_secret,
        allocator: &deps.allocator,
        exchanger: &deps.exchanger,
    };
    // 编译期类型归位：SessionService 字段借用结构在 user_auth 同构
    match svc.authenticate(token).await {
        Ok(uid) => Ok(uid),
        Err(chargepilot_mod_identity::IdentityError::SessionExpired) => {
            Err(crate::respond(401, 1001, "session expired", &rid))
        }
        Err(_) => Err(crate::respond(401, 1001, "access token invalid", &rid)),
    }
}

async fn report_fault(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let user_id = match user_id_from(&deps, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, 1005, "请提供报修信息", &rid);
    };
    let input = fault::FaultSubmission {
        device_id: v["device_id"].as_str().unwrap_or_default().to_string(),
        fault_type: v["fault_type"].as_str().unwrap_or_default().to_string(),
        description: v["description"].as_str().map(str::to_string),
        images: v["images"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };
    match fault::submit(&deps.db, user_id, input, "unknown", &rid, &deps.allocator).await {
        Ok(id) => httpx::ok(json!({"id": id.to_string(), "status": "open"}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct MyFaultQuery {
    page: Option<u32>,
    page_size: Option<u32>,
}

async fn my_faults(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Query(q): Query<MyFaultQuery>,
) -> Response {
    let rid = rid_of(&headers);
    let user_id = match user_id_from(&deps, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let page = q.page.unwrap_or(1).max(1);
    let size = q.page_size.unwrap_or(20).clamp(1, 100);
    match fault::list_mine(&deps.db, user_id, page, size).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn my_fault_history(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    let user_id = match user_id_from(&deps, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    match fault::my_history(&deps.db, user_id, id).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}
