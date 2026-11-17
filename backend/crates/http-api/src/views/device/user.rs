//! 设备域用户端门面。

//! 后台路由：设备管理、导入、故障工单、告警认领；
//! C 端路由：故障提交/查询、站点附近/详情（无需登录的部分公开）。

use crate::errormap::{device_error, rid_of, scan_error};
use crate::params::UserPaging;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::{fault, user_station};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

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
        return crate::respond(400, Code::BadRequest as i64, "分页参数无效", &rid);
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
            // 站点公告（global 生效窗口内，契约约定：详情页取公告列表）
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

async fn report_fault(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(user_id): UserGuard,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请提供报修信息", &rid);
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

async fn my_faults(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(user_id): UserGuard,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = rid_of(&headers);
    match fault::list_mine(&deps.db, user_id, page, page_size).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

async fn my_fault_history(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(user_id): UserGuard,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    match fault::my_history(&deps.db, user_id, id).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => device_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 匿名扫码
// ---------------------------------------------------------------------------

/// 端口/设备码合法字符集。
fn is_scan_code(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '-')
}

/// 裸码或 HTTPS 链接提取：
/// 裸码直接返回；否则要求 https 链接且 query 恰好一个合法 code 参数。
fn canonical_scan_code(raw: &str) -> Option<String> {
    if is_scan_code(raw) {
        return Some(raw.to_string());
    }
    if raw.len() > 2048
        || raw.contains('\u{0}')
        || raw.contains('\r')
        || raw.contains('\n')
        || raw.contains('\t')
    {
        return None;
    }
    let rest = raw.strip_prefix("https://")?;
    let (authority_and_path, query) = match rest.split_once('?') {
        Some((head, q)) => (head, q),
        None => return None,
    };
    // host 非空、无用户信息、无片段。
    if authority_and_path.is_empty() || authority_and_path.contains('@') {
        return None;
    }
    // query 中恰好一个 code 参数且值合法。
    let mut codes = Vec::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "code" {
            // + 与 %2B 形态的空格/加号差异由编码方保证；此处按原文校验字符集。
            codes.push(value.to_string());
        }
    }
    if codes.len() != 1 {
        return None;
    }
    let code = codes.pop()?;
    if is_scan_code(&code) {
        Some(code)
    } else {
        None
    }
}

/// 扫码错误映射。
/// 扫码解析：裸码或 HTTPS 二维码链接 → 设备或端口（匿名）。
async fn scan_resolve(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let code = body
        .map(|b| b.0)
        .and_then(|v| v.get("code").and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_default();
    let Some(code) = canonical_scan_code(&code) else {
        return crate::respond(400, Code::BadRequest as i64, "invalid scan code", &rid);
    };
    match chargepilot_mod_device::scan::resolve(&deps.db, &code).await {
        Ok(view) => httpx::ok(serde_json::to_value(&view).unwrap_or(Value::Null), &rid),
        Err(e) => scan_error(e, &rid),
    }
}

/// 端口查询：端口编码 → 单端口（匿名；命中设备码按输出 port not found）。
async fn scan_port(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let port_id = body
        .map(|b| b.0)
        .and_then(|v| {
            v.get("port_id")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if !is_scan_code(&port_id) {
        return crate::respond(400, Code::BadRequest as i64, "invalid scan code", &rid);
    }
    match chargepilot_mod_device::scan::resolve(&deps.db, &port_id).await {
        Ok(view) if view.kind == "port" && view.port.is_some() => {
            httpx::ok(serde_json::to_value(view.port).unwrap_or(Value::Null), &rid)
        }
        Ok(_) => crate::respond(404, Code::NotFound as i64, "port not found", &rid),
        Err(e) => scan_error(e, &rid),
    }
}
/// 用户端站点/报修/扫码路由。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
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
        .route(
            "/api/v1/user/scan/resolve",
            axum::routing::post(scan_resolve),
        )
        .route("/api/v1/user/scan/port", axum::routing::post(scan_port))
}
