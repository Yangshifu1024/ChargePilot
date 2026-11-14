//! 运营域后台门面·审计与 MFA（票 18：原 admin_misc.rs 的审计/MFA 区块）。
use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_mod_ops::mfa::MfaError;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

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
        Err(e) => return crate::respond(400, Code::BadRequest as i64, &e.to_string(), &rid),
    };
    // 仅解析 RFC3339 输入（旧版 time.Parse(time.RFC3339)；无效返回 400）。
    let parse_bound =
        |raw: Option<&str>| -> Result<Option<chrono::DateTime<chrono::Utc>>, Response> {
            match raw.filter(|s| !s.is_empty()) {
                None => Ok(None),
                Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
                    Ok(t) => Ok(Some(t.with_timezone(&chrono::Utc))),
                    Err(_) => Err(crate::respond(
                        400,
                        Code::BadRequest as i64,
                        "时间须为 RFC 3339 格式",
                        &rid,
                    )),
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
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                &rid,
            )
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
        MfaError::Input | MfaError::InputMsg(_) => {
            crate::respond(400, Code::BadRequest as i64, &e.to_string(), rid)
        }
        MfaError::Proof => crate::respond(
            400,
            Code::VerificationFailed as i64,
            "密码或验证码校验失败",
            rid,
        ),
        MfaError::UsedTotp => crate::respond(
            400,
            Code::VerificationFailed as i64,
            "该验证码已使用，请等待下一个验证码",
            rid,
        ),
        MfaError::Pending | MfaError::State | MfaError::Busy => {
            crate::respond(409, Code::Conflict as i64, &e.to_string(), rid)
        }
        MfaError::Rate => {
            let mut resp = crate::respond(
                429,
                Code::LoginRateLimited as i64,
                "安全验证过于频繁，请稍后重试",
                rid,
            );
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("60"));
            resp
        }
        MfaError::SelfMfa => crate::respond(
            409,
            Code::Conflict as i64,
            "请使用本人账号的安全设置修改双因素认证",
            rid,
        ),
        MfaError::Credentials => crate::respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            rid,
        ),
        MfaError::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        MfaError::Storage(_) => {
            tracing::warn!(cause = %e, "MFA 请求处理失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// 方案预览与设备能力
/// 审计与 MFA 路由（票 18：原 admin_misc 区块归位）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/audit-logs",
            axum::routing::get(list_audit_logs),
        )
        .route(
            "/api/v1/admin/admin-users/{id}/mfa",
            axum::routing::post(admin_user_mfa),
        )
        .route(
            "/api/v1/admin/auth/mfa-settings",
            axum::routing::get(self_mfa_status).post(self_mfa_settings),
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
