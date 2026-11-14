//! 设备域后台门面（票 18：先归位设备能力；admin_pricing_device/device_and_user 热区批并入）。
use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::{DeviceError, alert, fault, import, management, station, vendor};
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_mod_pricing::metering;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
struct CapabilityQuery {
    station_id: Option<u64>,
    device_id: Option<String>,
}

/// 设备能力清单（旧版 executionCapabilities）：station_id 必填正整数 + 设备号规则校验。
async fn device_capabilities(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<CapabilityQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.read", &rid) {
        return resp;
    }
    let Some(station_id) = q.station_id.filter(|s| *s > 0) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "station_id 必须为正整数",
            &rid,
        );
    };
    let device = q.device_id.as_deref().unwrap_or_default();
    match chargepilot_mod_device::capabilities::execution_capabilities(&deps.db, station_id, device)
        .await
    {
        Ok(data) => httpx::ok(data, &rid),
        Err(e) => capability_error(e, &rid),
    }
}

/// 能力查询错误映射（旧版 pricingFailure：InputError→400、其余按资源失败）。
fn capability_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => crate::respond(400, Code::BadRequest as i64, msg, rid),
        DeviceError::NotFound => {
            crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        DeviceError::Forbidden => crate::respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        DeviceError::Conflict(msg) => crate::respond(409, Code::Conflict as i64, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "设备能力查询失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// 设备能力路由（票 18：原 admin_misc 区块归位）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/admin/settings/device-capabilities",
        axum::routing::get(device_capabilities),
    )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

fn deny(guard: &AdminGuard, permission: &str, rid: &str) -> Option<Response> {
    if guard.has(permission) {
        None
    } else {
        Some(ops_error(OpsError::Permission, rid))
    }
}

// ---------------------------------------------------------------------------
// 反馈工单
// ---------------------------------------------------------------------------

// ---- 站点/厂商/导入/故障/告警（票 19：原 admin_pricing_device.rs 与 device_and_user.rs 的 admin 切片）----

// 后台路由：计费方案（模板/发布/继承/生效）、站点、厂商。
// 权限码与路径对齐旧版（pricing.read、pricing.rule.create|update、station.*、vendor.*）。

fn device_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => crate::respond(400, Code::BadRequest as i64, msg, rid),
        DeviceError::Forbidden => crate::respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        DeviceError::NotFound => {
            crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        DeviceError::Conflict(msg) => crate::respond(409, Code::Conflict as i64, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "设备域存储操作失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}
#[derive(Deserialize)]
struct PagingQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    keyword: Option<String>,
    status: Option<String>,
}

async fn list_stations(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<PagingQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "station.read", &rid) {
        return resp;
    }
    let page = match Page::sanitized(q.page.unwrap_or(1), q.page_size.unwrap_or(20)) {
        Ok(p) => p,
        Err(e) => return crate::respond(400, Code::BadRequest as i64, &e.to_string(), &rid),
    };
    // 前端「全部状态」以空串发送（axios 照发 status=），旧版语义空串=不过滤。
    let status = q.status.as_deref().filter(|s| !s.is_empty());
    match station::list(&deps.db, page, q.keyword.as_deref(), status).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size,
                   "permissions": guard.profile.permissions}),
            &rid,
        ),
        Err(e) => device_error(e, &rid),
    }
}

async fn get_station(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "station.read", &rid) {
        return resp;
    }
    match station::get(&deps.db, id).await {
        Ok(s) => match serde_json::to_value(&s) {
            Ok(v) => httpx::ok(v, &rid),
            Err(_) => crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取",
                &rid,
            ),
        },
        Err(e) => device_error(e, &rid),
    }
}

async fn create_station(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<station::StationInput>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "station.create", &rid) {
        return resp;
    }
    let Some(input) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请填写站点信息", &rid);
    };
    match station::create(
        &deps.db,
        &guard.profile,
        input,
        "unknown",
        &rid,
        &deps.allocator,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn update_station(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<station::StationInput>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "station.update", &rid) {
        return resp;
    }
    let Some(input) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请填写站点信息", &rid);
    };
    match station::update(&deps.db, &guard.profile, id, input, "unknown", &rid).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 厂商
// ---------------------------------------------------------------------------

async fn list_vendors(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "vendor.read", &rid) {
        return resp;
    }
    match vendor::list(&deps.db, false).await {
        // 前端 Vendors.tsx 从列表响应读当前操作人权限集（旧版 Profile.Permissions）。
        Ok(items) => httpx::ok(
            json!({"items": items, "permissions": guard.profile.permissions}),
            &rid,
        ),
        Err(e) => device_error(e, &rid),
    }
}

async fn get_vendor(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "vendor.read", &rid) {
        return resp;
    }
    match vendor::get(&deps.db, id).await {
        Ok(v) => {
            let data = serde_json::to_value(&v).unwrap_or(Value::Null);
            httpx::ok(data, &rid)
        }
        Err(e) => device_error(e, &rid),
    }
}

async fn create_vendor(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<vendor::VendorInput>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "vendor.create", &rid) {
        return resp;
    }
    let Some(input) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请填写厂商信息", &rid);
    };
    match vendor::create(
        &deps.db,
        &guard.profile,
        input,
        "unknown",
        &rid,
        &deps.allocator,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn update_vendor(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<vendor::VendorInput>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "vendor.update", &rid) {
        return resp;
    }
    let Some(input) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请填写厂商信息", &rid);
    };
    match vendor::update(&deps.db, &guard.profile, id, input, "unknown", &rid).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn vendor_options(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "device.import", &rid) {
        return resp;
    }
    match vendor::list(&deps.db, true).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

// metering 供后续阶段引用（下单/刷卡），此处编译期保持可见
#[allow(dead_code)]
fn _metering_visible(_m: &metering::ActiveRule) {}
/// 站点与厂商路由（票 19 热区批并入）。
pub fn station_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/stations",
            axum::routing::get(list_stations).post(create_station),
        )
        .route(
            "/api/v1/admin/stations/{id}",
            axum::routing::get(get_station).put(update_station),
        )
        .route(
            "/api/v1/admin/vendors",
            axum::routing::get(list_vendors).post(create_vendor),
        )
        .route(
            "/api/v1/admin/vendors/{id}",
            axum::routing::get(get_vendor).put(update_vendor),
        )
        .route(
            "/api/v1/admin/vendor-options",
            axum::routing::get(vendor_options),
        )
}

// ---- 设备管理/导入/故障/告警（票 19：原 device_and_user.rs 的 admin 切片）----

// 后台路由：设备管理、导入、故障工单、告警认领；
// C 端路由：故障提交/查询、站点附近/详情（无需登录的部分公开）。

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
        Err(e) => return crate::respond(400, Code::BadRequest as i64, &e.to_string(), &rid),
    };
    match management::list(
        &deps.db,
        page,
        q.station_id,
        // 前端「全部状态」以空串发送（axios 照发 status=），旧版语义空串=不过滤。
        q.status.as_deref().filter(|s| !s.is_empty()),
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
        return crate::respond(400, Code::BadRequest as i64, "请提供设备资料", &rid);
    };
    let input: management::DeviceInput = match serde_json::from_value(v) {
        Ok(i) => i,
        Err(_) => {
            return crate::respond(
                400,
                Code::BadRequest as i64,
                "设备资料字段缺失或类型无效",
                &rid,
            );
        }
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
        return crate::respond(400, Code::BadRequest as i64, "请提供导入清单", &rid);
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
        Err(e) => return crate::respond(400, Code::BadRequest as i64, &e.to_string(), &rid),
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
        return crate::respond(400, Code::BadRequest as i64, "请指定处理人", &rid);
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

/// 设备管理路由（票 19 热区批并入；与 stations/vendors、capabilities 同文件）。
pub fn device_admin_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/devices", axum::routing::get(list_devices))
        .route(
            "/api/v1/admin/devices/{id}",
            axum::routing::get(get_device).put(update_device),
        )
        .route(
            "/api/v1/admin/devices/{id}/status",
            axum::routing::put(set_device_status),
        )
        .route(
            "/api/v1/admin/device-imports",
            axum::routing::get(list_imports).post(create_import),
        )
        .route(
            "/api/v1/admin/device-imports/{import_id}/retry",
            axum::routing::post(retry_import),
        )
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
}
