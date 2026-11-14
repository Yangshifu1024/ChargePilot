//! 设备域后台门面（票 18：先归位设备能力；admin_pricing_device/device_and_user 热区批并入）。
use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use crate::errormap::ops_error;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::DeviceError;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::httpx;
use serde::Deserialize;
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
        return crate::respond(400, 1005, "station_id 必须为正整数", &rid);
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
        DeviceError::Input(msg) => crate::respond(400, 1005, msg, rid),
        DeviceError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        DeviceError::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        DeviceError::Conflict(msg) => crate::respond(409, 2009, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "设备能力查询失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
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
