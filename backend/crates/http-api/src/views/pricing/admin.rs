//! 定价域后台门面（票 18：先归位方案预览；admin_pricing_device 热区批并入）。
use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use crate::errormap::ops_error;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::httpx;
use serde_json::Value;
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
        return crate::respond(400, 1005, "请提供完整的计费方案与模拟计量", &rid);
    };
    let scheme: chargepilot_domain::charging_scheme::Scheme =
        match serde_json::from_value(v["scheme"].clone()) {
            Ok(s) => s,
            Err(e) => return crate::respond(400, 1005, &format!("方案无效：{e}"), &rid),
        };
    let meter: chargepilot_domain::charging_actual::ActualMeter =
        match serde_json::from_value(v["meter"].clone()) {
            Ok(m) => m,
            Err(e) => return crate::respond(400, 1005, &format!("模拟计量无效：{e}"), &rid),
        };
    let package_id = v["package_id"].as_u64().unwrap_or(0);
    let scenario = v["scenario"].as_str().unwrap_or_default();
    let wallet_cents = v["wallet_cents"].as_i64().unwrap_or(0);
    // 旧版 Scheme.PreviewScenario 的零值 package_id 与计量缺省行为一致。
    let meter = normalize_meter(meter);
    match scheme.preview_scenario(package_id, meter, scenario, wallet_cents) {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(data) => httpx::ok(data, &rid),
            Err(_) => crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", &rid),
        },
        Err(e) => crate::respond(400, 1005, &e.0, &rid),
    }
}

/// 补齐计量缺省：起止时刻缺省时以当前时刻构造零时长会话（旧版 ActualMeter 零值语义）。
fn normalize_meter(
    mut meter: chargepilot_domain::charging_actual::ActualMeter,
) -> chargepilot_domain::charging_actual::ActualMeter {
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
