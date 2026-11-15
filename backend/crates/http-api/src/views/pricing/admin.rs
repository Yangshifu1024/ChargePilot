//! 定价域后台门面（票 18：先归位方案预览；admin_pricing_device 热区批并入）。
use crate::errormap::pricing_error;
use crate::errormap::rid_of;
use crate::guard::AdminGuard;
use crate::guard::deny;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_pricing::{publication, templates};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde_json::{Value, json};
use std::sync::Arc;

async fn preview_scheme(
    State(_deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "pricing.read", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请提供完整的计费方案与模拟计量",
            &rid,
        );
    };
    let scheme: chargepilot_domain::charging::scheme::Scheme =
        match serde_json::from_value(v["scheme"].clone()) {
            Ok(s) => s,
            Err(e) => {
                return crate::respond(
                    400,
                    Code::BadRequest as i64,
                    &format!("方案无效：{e}"),
                    &rid,
                );
            }
        };
    let meter: chargepilot_domain::charging::actual::ActualMeter =
        match serde_json::from_value(v["meter"].clone()) {
            Ok(m) => m,
            Err(e) => {
                return crate::respond(
                    400,
                    Code::BadRequest as i64,
                    &format!("模拟计量无效：{e}"),
                    &rid,
                );
            }
        };
    let package_id = v["package_id"].as_u64().unwrap_or(0);
    let scenario = v["scenario"].as_str().unwrap_or_default();
    let wallet_cents = v["wallet_cents"].as_i64().unwrap_or(0);
    // 旧版 Scheme.PreviewScenario 的零值 package_id 与计量缺省行为一致。
    let meter = normalize_meter(meter);
    match scheme.preview_scenario(package_id, meter, scenario, wallet_cents) {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(data) => httpx::ok(data, &rid),
            Err(_) => crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                &rid,
            ),
        },
        Err(e) => crate::respond(400, Code::BadRequest as i64, &e.0, &rid),
    }
}

/// 补齐计量缺省：起止时刻缺省时以当前时刻构造零时长会话（旧版 ActualMeter 零值语义）。
fn normalize_meter(
    mut meter: chargepilot_domain::charging::actual::ActualMeter,
) -> chargepilot_domain::charging::actual::ActualMeter {
    let now = chrono::Utc::now();
    if meter.started_at.is_none() {
        meter.started_at = Some(now);
    }
    if meter.ended_at.is_none() {
        meter.ended_at = Some(now);
    }
    meter
}

/// 方案预览路由（票 18：原 admin_misc 区块归位）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/admin/settings/charging-schemes/preview",
        axum::routing::post(preview_scheme),
    )
}

// ---------------------------------------------------------------------------
// 反馈工单
// ---------------------------------------------------------------------------

// ---- 计费方案管理（票 19：原 admin_pricing_device.rs 的 schemes 切片）----

// 后台路由：计费方案（模板/发布/继承/生效）、站点、厂商。
// 权限码与路径对齐旧版（pricing.read、pricing.rule.create|update、station.*、vendor.*）。

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
        return crate::respond(400, Code::BadRequest as i64, "请提供完整的计费方案", &rid);
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
        return crate::respond(400, Code::BadRequest as i64, "请提供完整的计费方案", &rid);
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
        return crate::respond(400, Code::BadRequest as i64, "请提供发布目标与方案", &rid);
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
        return crate::respond(400, Code::BadRequest as i64, "请提供继承目标设备", &rid);
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
/// 计费方案路由（票 19 热区批并入；与 preview 路由同文件）。
pub fn scheme_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
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
}
