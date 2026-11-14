//! 订单域后台门面（票 18：先归位反馈工单区块；admin_orders 热区批并入）。
use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use crate::errormap::ops_error;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::Page;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

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

/// 反馈工单路由（票 18：原 admin_misc 区块归位）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/feedback", axum::routing::get(list_feedback))
        .route(
            "/api/v1/admin/feedback/{id}/reply",
            axum::routing::post(reply_feedback),
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
