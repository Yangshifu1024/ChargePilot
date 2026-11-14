//! 后台路由：计费方案（模板/发布/继承/生效）、站点、厂商。
//! 权限码与路径对齐旧版（pricing.read、pricing.rule.create|update、station.*、vendor.*）。

use crate::admin_auth::ops_error;
use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::{DeviceError, station, vendor};
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_mod_pricing::{PricingError, metering, publication, templates};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        // 计费方案
        .route(
            "/api/v1/admin/settings/charging-schemes",
            axum::routing::get(list_schemes).post(create_scheme),
        )
        .route(
            "/api/v1/admin/settings/charging-schemes/{id}",
            axum::routing::put(update_scheme).delete(delete_scheme),
        )
        .route(
            "/api/v1/admin/settings/charging-schemes/{id}/status",
            axum::routing::post(scheme_status),
        )
        .route(
            "/api/v1/admin/settings/charging-schemes/apply",
            axum::routing::post(apply_scheme),
        )
        .route(
            "/api/v1/admin/stations/{id}/charging-scheme",
            axum::routing::get(effective_scheme),
        )
        .route(
            "/api/v1/admin/stations/{id}/charging-scheme/inherit",
            axum::routing::post(inherit_scheme),
        )
        // 站点
        .route(
            "/api/v1/admin/stations",
            axum::routing::get(list_stations).post(create_station),
        )
        .route(
            "/api/v1/admin/stations/{id}",
            axum::routing::get(get_station).put(update_station),
        )
        // 厂商
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

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

fn pricing_error(e: PricingError, rid: &str) -> Response {
    match &e {
        PricingError::InvalidScheme(msg) => crate::respond(409, 2009, msg, rid),
        PricingError::VersionConflict | PricingError::PublishMismatch => {
            crate::respond(409, 2009, &e.to_string(), rid)
        }
        PricingError::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        PricingError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        PricingError::RuleUnavailable => crate::respond(409, 2009, &e.to_string(), rid),
        PricingError::Storage(_) => {
            tracing::warn!(cause = %e, "pricing 存储失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
        }
    }
}

fn device_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => crate::respond(400, 1005, msg, rid),
        DeviceError::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        DeviceError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        DeviceError::Conflict(msg) => crate::respond(409, 2009, msg, rid),
        DeviceError::Storage(_) => {
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

// ---------------------------------------------------------------------------
// 计费方案
// ---------------------------------------------------------------------------

async fn list_schemes(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.read", &rid) {
        return resp;
    }
    match templates::list_templates(&deps.db).await {
        Ok(items) => httpx::ok(
            json!({"items": items, "permissions": guard.profile.permissions}),
            &rid,
        ),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn create_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.create", &rid) {
        return resp;
    }
    let Some(input) = body
        .map(|b| b.0)
        .and_then(|v| serde_json::from_value::<templates::SchemeInput>(v).ok())
    else {
        return crate::respond(400, 1005, "请提供完整的计费方案", &rid);
    };
    match templates::create_template(
        &deps.db,
        &guard.profile,
        input,
        "unknown",
        &rid,
        &deps.allocator,
    )
    .await
    {
        Ok((id, version)) => httpx::ok(json!({"id": id, "version": version}), &rid),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn update_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.update", &rid) {
        return resp;
    }
    let Some(input) = body
        .map(|b| b.0)
        .and_then(|v| serde_json::from_value::<templates::SchemeInput>(v).ok())
    else {
        return crate::respond(400, 1005, "请提供完整的计费方案", &rid);
    };
    match templates::update_template(&deps.db, &guard.profile, id, input, "unknown", &rid).await {
        Ok((id, version)) => httpx::ok(json!({"id": id, "version": version}), &rid),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn scheme_status(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.update", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    match templates::set_template_status(
        &deps.db,
        &guard.profile,
        id,
        v["expected_version"].as_u64().unwrap_or(0),
        v["status"].as_str(),
        false,
        "unknown",
        &rid,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn delete_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.update", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    match templates::set_template_status(
        &deps.db,
        &guard.profile,
        id,
        v["expected_version"].as_u64().unwrap_or(0),
        None,
        true,
        "unknown",
        &rid,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn apply_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.create", &rid) {
        return resp;
    }
    let Some(input) = body
        .map(|b| b.0)
        .and_then(|v| serde_json::from_value::<publication::PublishInput>(v).ok())
    else {
        return crate::respond(400, 1005, "请提供发布目标与方案", &rid);
    };
    match publication::publish(&deps.db, &guard.profile, input, "unknown", &rid).await {
        Ok((id, version, replayed)) => httpx::ok(
            json!({"id": id, "version": version, "replayed": replayed}),
            &rid,
        ),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn effective_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.read", &rid) {
        return resp;
    }
    match publication::effective_scheme(&deps.db, id, q.get("device_id").map(String::as_str)).await
    {
        Ok(e) => httpx::ok(
            json!({"scheme": e.scheme, "version": e.version, "rule_id": e.rule_id, "inherited": e.inherited}),
            &rid,
        ),
        Err(e) => pricing_error(e, &rid),
    }
}

async fn inherit_scheme(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.rule.update", &rid) {
        return resp;
    }
    let Some(input) = body
        .map(|b| b.0)
        .and_then(|v| serde_json::from_value::<publication::InheritInput>(v).ok())
    else {
        return crate::respond(400, 1005, "请提供继承目标设备", &rid);
    };
    match publication::inherit_station_rule(&deps.db, &guard.profile, id, input, "unknown", &rid)
        .await
    {
        Ok(()) => httpx::ok(json!({"inherited": true}), &rid),
        Err(e) => pricing_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 站点
// ---------------------------------------------------------------------------

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
        Err(e) => return crate::respond(400, 1005, &e.to_string(), &rid),
    };
    match station::list(&deps.db, page, q.keyword.as_deref(), q.status.as_deref()).await {
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
            Err(_) => crate::respond(503, 5003, "数据暂时无法读取", &rid),
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
        return crate::respond(400, 1005, "请填写站点信息", &rid);
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
        return crate::respond(400, 1005, "请填写站点信息", &rid);
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
        return crate::respond(400, 1005, "请填写厂商信息", &rid);
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
        return crate::respond(400, 1005, "请填写厂商信息", &rid);
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
