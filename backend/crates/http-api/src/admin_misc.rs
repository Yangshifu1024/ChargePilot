//! 后台零散路由（批次补齐 7 端点）：反馈工单、审计日志、管理员 MFA、
//! 本人 MFA 设置、方案预览试算与设备能力。路径/权限码/响应形状对齐旧版
//! （casework.go / audit_log.go / admin_accounts.go / admin_mfa_settings.go /
//! charging_schemes.go 的 preview 与 executionCapabilities）。

use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use crate::errormap::ops_error;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_device::DeviceError;
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_mod_ops::mfa::MfaError;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        // 反馈工单（旧版 registerCasework 的 feedback 两端点）
        .route("/api/v1/admin/feedback", axum::routing::get(list_feedback))
        .route(
            "/api/v1/admin/feedback/{id}/reply",
            axum::routing::post(reply_feedback),
        )
        // 审计日志
        .route(
            "/api/v1/admin/audit-logs",
            axum::routing::get(list_audit_logs),
        )
        // 管理端 MFA 重置与本人 MFA 设置
        .route(
            "/api/v1/admin/admin-users/{id}/mfa",
            axum::routing::post(admin_user_mfa),
        )
        .route(
            "/api/v1/admin/auth/mfa-settings",
            axum::routing::get(self_mfa_status).post(self_mfa_settings),
        )
        // 方案预览与设备能力
        .route(
            "/api/v1/admin/settings/charging-schemes/preview",
            axum::routing::post(preview_scheme),
        )
        .route(
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

fn real_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// 反馈工单
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CasePaging {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
}

/// 反馈状态白名单（旧版 ReadAdminPage(c, "pending processed closed")）。
async fn list_feedback(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<CasePaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "feedback.read", &rid) {
        return resp;
    }
    // 状态过滤白名单语义收编共享提取器（票 17）；错误文案逐字保留。
    if crate::params::StatusFilter::allowed(
        q.status.as_deref(),
        &["pending", "processed", "closed"],
        &rid,
        "关键词过长或状态无效",
    )
    .is_err()
    {
        return crate::respond(400, 1005, "关键词过长或状态无效", &rid);
    }
    let page = match Page::sanitized(q.page.unwrap_or(1), q.page_size.unwrap_or(20)) {
        Ok(p) => p,
        Err(e) => return crate::respond(400, 1005, &e.to_string(), &rid),
    };
    match chargepilot_mod_order::feedback::list(
        &deps.db,
        q.status.as_deref().unwrap_or(""),
        page.page,
        page.page_size,
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size}),
            &rid,
        ),
        Err(e) => feedback_error(e, &rid),
    }
}

async fn reply_feedback(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "feedback.reply", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let action = v["action"].as_str().unwrap_or_default().to_string();
    let reply_content = v["reply_content"].as_str().unwrap_or_default().to_string();
    match chargepilot_mod_order::feedback::reply(
        &deps.db,
        &guard.profile,
        id,
        &action,
        &reply_content,
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok(()) => httpx::ok(json!({"saved": true}), &rid),
        Err(e) => feedback_error(e, &rid),
    }
}

/// 反馈错误映射（旧版 AdminResourceFailure + FeedbackInputError）。
fn feedback_error(e: chargepilot_mod_order::feedback::FeedbackError, rid: &str) -> Response {
    use chargepilot_mod_order::feedback::FeedbackError as E;
    match &e {
        E::Input(msg) => crate::respond(400, 1005, msg, rid),
        E::Conflict => crate::respond(409, 2009, "当前记录状态不允许此操作，请刷新后重试", rid),
        E::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        E::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        E::Storage(_) => {
            tracing::warn!(cause = %e, "反馈请求处理失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
        }
    }
}

// ---------------------------------------------------------------------------
// 审计日志
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuditPaging {
    page: Option<i64>,
    page_size: Option<i64>,
    module: Option<String>,
    action: Option<String>,
    actor: Option<String>,
    target_type: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

async fn list_audit_logs(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<AuditPaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "audit.read", &rid) {
        return resp;
    }
    let page = match Page::sanitized(q.page.unwrap_or(1), q.page_size.unwrap_or(20)) {
        Ok(p) => p,
        Err(e) => return crate::respond(400, 1005, &e.to_string(), &rid),
    };
    // 仅解析 RFC3339 输入（旧版 time.Parse(time.RFC3339)；无效返回 400）。
    let parse_bound =
        |raw: Option<&str>| -> Result<Option<chrono::DateTime<chrono::Utc>>, Response> {
            match raw.filter(|s| !s.is_empty()) {
                None => Ok(None),
                Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
                    Ok(t) => Ok(Some(t.with_timezone(&chrono::Utc))),
                    Err(_) => Err(crate::respond(400, 1005, "时间须为 RFC 3339 格式", &rid)),
                },
            }
        };
    let from = match parse_bound(q.from.as_deref()) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let to = match parse_bound(q.to.as_deref()) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let query = chargepilot_mod_ops::audit_query::AuditQuery {
        module: q.module.unwrap_or_default(),
        action: q.action.unwrap_or_default(),
        actor: q.actor.unwrap_or_default(),
        target_type: q.target_type.unwrap_or_default(),
        from,
        to,
    };
    match chargepilot_mod_ops::audit_query::list(&deps.db, &query, page.page, page.page_size).await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size}),
            &rid,
        ),
        Err(e) => {
            tracing::warn!(cause = %e, "审计日志读取失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", &rid)
        }
    }
}

// ---------------------------------------------------------------------------
// 管理端 MFA 与本人 MFA 设置
// ---------------------------------------------------------------------------

async fn admin_user_mfa(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.update", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    match chargepilot_mod_ops::mfa::managed(
        &deps.db,
        &guard.profile,
        id,
        v["action"].as_str().unwrap_or_default(),
        v["secret"].as_str().unwrap_or_default(),
        v["code"].as_str().unwrap_or_default(),
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok(reply) => httpx::ok(reply, &rid),
        Err(e) => mfa_error(e, &rid),
    }
}

/// 当前管理员 MFA 状态（GET；前端 Security 页读取）。
async fn self_mfa_status(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_ops::mfa::status(&deps.db, guard.profile.id).await {
        Ok(reply) => httpx::ok(reply, &rid),
        Err(e) => mfa_error(e, &rid),
    }
}

/// 本人 MFA 设置（POST；动作 enrol/confirm/disable，旧版 mfaSettings 契约）。
async fn self_mfa_settings(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    match chargepilot_mod_ops::mfa::apply_settings(
        &deps.db,
        &deps.redis_url,
        &guard.profile,
        &guard.sid,
        v["action"].as_str().unwrap_or_default(),
        v["password"].as_str().unwrap_or_default(),
        v["code"].as_str().unwrap_or_default(),
        v["enrollment_id"].as_str().unwrap_or_default(),
        &real_ip(&headers),
    )
    .await
    {
        Ok(reply) => httpx::ok(reply, &rid),
        Err(e) => mfa_error(e, &rid),
    }
}

/// MFA 错误映射（旧版 mfaSettingsFailure + AdminManagementFailure）。
fn mfa_error(e: MfaError, rid: &str) -> Response {
    match &e {
        MfaError::Input | MfaError::InputMsg(_) => crate::respond(400, 1005, &e.to_string(), rid),
        MfaError::Proof => crate::respond(400, 1002, "密码或验证码校验失败", rid),
        MfaError::UsedTotp => crate::respond(400, 1002, "该验证码已使用，请等待下一个验证码", rid),
        MfaError::Pending | MfaError::State | MfaError::Busy => {
            crate::respond(409, 2009, &e.to_string(), rid)
        }
        MfaError::Rate => {
            let mut resp = crate::respond(429, 4291, "安全验证过于频繁，请稍后重试", rid);
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("60"));
            resp
        }
        MfaError::SelfMfa => {
            crate::respond(409, 2009, "请使用本人账号的安全设置修改双因素认证", rid)
        }
        MfaError::Credentials => crate::respond(401, 1001, "用户名或密码错误，或登录已失效", rid),
        MfaError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        MfaError::Storage(_) => {
            tracing::warn!(cause = %e, "MFA 请求处理失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
        }
    }
}

// ---------------------------------------------------------------------------
// 方案预览与设备能力
// ---------------------------------------------------------------------------

/// 方案预览（旧版 previewChargingScheme）：请求携带完整方案与模拟计量，不读取数据库；
/// 领域引擎试算失败按 400 透出文案（旧版 httpapi.BadRequest(err.Error())）。
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
