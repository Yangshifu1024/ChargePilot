//! 营销域后台门面（票 18 冷区批：原 admin_marketing.rs 归位）。

use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_domain::marketing::ActivityRuleInput;
use chargepilot_mod_marketing::CouponError;
use chargepilot_mod_marketing::activity::{
    create_activity_rule, has_activity_read, list_activity_rules, update_activity_rule,
};
use chargepilot_mod_marketing::coupon::{
    CouponRow, CouponUpdate, GrantInput, coupon_stats, create_coupon, grant_coupon, list_coupons,
    update_coupon,
};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/coupons",
            axum::routing::get(list_coupons_handler).post(create_coupon_handler),
        )
        .route(
            "/api/v1/admin/coupons/{id}",
            axum::routing::put(update_coupon_handler),
        )
        .route(
            "/api/v1/admin/coupons/{id}/stats",
            axum::routing::get(coupon_stats_handler),
        )
        .route(
            "/api/v1/admin/coupons/{id}/grants",
            axum::routing::post(grant_coupon_handler),
        )
        .route(
            "/api/v1/admin/coupon-activities",
            axum::routing::get(list_activities).post(create_activity),
        )
        .route(
            "/api/v1/admin/coupon-activities/{id}",
            axum::routing::put(update_activity),
        )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

fn real_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

fn deny(guard: &AdminGuard, permission: &str, rid: &str) -> Option<Response> {
    if guard.has(permission) {
        None
    } else {
        Some(crate::respond(
            403,
            Code::Forbidden as i64,
            "无权执行此操作",
            rid,
        ))
    }
}

/// 券域错误 → HTTP（映射对齐旧版 failure + AdminResourceFailure）。
fn coupon_error(e: CouponError, rid: &str) -> Response {
    match &e {
        CouponError::Input(m) => crate::respond(400, Code::BadRequest as i64, m, rid),
        CouponError::Forbidden => {
            crate::respond(403, Code::Forbidden as i64, "无权执行此操作", rid)
        }
        CouponError::Conflict(m) => crate::respond(409, Code::Conflict as i64, m, rid),
        CouponError::NotFound => {
            crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        CouponError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券请求处理失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

async fn list_coupons_handler(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.read", &rid) {
        return resp;
    }
    match list_coupons(&deps.db, &guard.profile).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => coupon_error(e, &rid),
    }
}

async fn create_coupon_handler(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.create", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "优惠券名称、有效期或额度无效",
            &rid,
        );
    };
    let in_row = CouponRow {
        id: v["id"].as_u64().unwrap_or(0),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        discount_type: v["discount_type"].as_str().unwrap_or_default().to_string(),
        discount_value_cents: v["discount_value_cents"].as_i64(),
        discount_percent: v["discount_percent"].as_f64(),
        free_minutes: v["free_minutes"].as_u64().map(|x| x as u32),
        min_charge_cents: v["min_charge_cents"].as_i64().unwrap_or(0),
        valid_hours: v["valid_hours"].as_u64().unwrap_or(0) as u32,
        total_quota: v["total_quota"].as_u64().unwrap_or(0) as u32,
        per_user_quota: v["per_user_quota"].as_u64().unwrap_or(0) as u32,
        // 创建时状态由服务端强制 active（旧版忽略提交值），这里仅占位。
        status: String::new(),
        start_at: serde_json::from_value(v["start_at"].clone()).unwrap_or(None),
        end_at: serde_json::from_value(v["end_at"].clone()).unwrap_or(None),
    };
    let ip = real_ip(&headers);
    match create_coupon(&deps.db, &guard.profile, &ip, &rid, in_row).await {
        Ok(row) => {
            let body = serde_json::to_value(&row).unwrap_or(Value::Null);
            httpx::ok(body, &rid)
        }
        Err(e) => coupon_error(e, &rid),
    }
}

async fn update_coupon_handler(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.update", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "名称或状态无效", &rid);
    };
    let update = CouponUpdate {
        name: v["name"].as_str().unwrap_or_default().to_string(),
        status: v["status"].as_str().unwrap_or_default().to_string(),
    };
    let ip = real_ip(&headers);
    match update_coupon(&deps.db, &guard.profile, &ip, &rid, id, update).await {
        Ok(out) => httpx::ok(json!({"name": out.name, "status": out.status}), &rid),
        Err(e) => coupon_error(e, &rid),
    }
}

async fn coupon_stats_handler(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.read", &rid) {
        return resp;
    }
    match coupon_stats(&deps.db, &guard.profile, id).await {
        Ok(stats) => {
            let body = serde_json::to_value(&stats).unwrap_or(Value::Null);
            httpx::ok(body, &rid)
        }
        Err(e) => coupon_error(e, &rid),
    }
}

async fn grant_coupon_handler(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.grant", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "请求编号或用户编号无效", &rid);
    };
    let input = GrantInput {
        request_id: v["request_id"].as_str().unwrap_or_default().to_string(),
        // 旧版 DTO 为十进制字符串（httpapi.DecimalID，避免 JS 精度丢失），兼容数字。
        user_id: v["user_id"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v["user_id"].as_u64())
            .unwrap_or(0),
    };
    let ip = real_ip(&headers);
    match grant_coupon(&deps.db, &guard.profile, &ip, &rid, id, input).await {
        Ok(out) => {
            let body = serde_json::to_value(&out).unwrap_or(Value::Null);
            httpx::ok(body, &rid)
        }
        Err(e) => coupon_error(e, &rid),
    }
}

/// 活动列表查询参数（旧版 ReadAdminPage：page/page_size/status/keyword）。
#[derive(Deserialize)]
struct ActivityListQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    keyword: Option<String>,
}

async fn list_activities(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<ActivityListQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if !has_activity_read(&guard.profile) {
        return crate::respond(403, Code::Forbidden as i64, "无权执行此操作", &rid);
    }
    match list_activity_rules(
        &deps.db,
        &guard.profile,
        q.status.as_deref().unwrap_or(""),
        q.keyword.as_deref().unwrap_or(""),
        q.page.unwrap_or(1).max(1),
        q.page_size.unwrap_or(20).clamp(1, 100),
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": q.page.unwrap_or(1).max(1),
                   "page_size": q.page_size.unwrap_or(20).clamp(1, 100)}),
            &rid,
        ),
        Err(e) => coupon_error(e, &rid),
    }
}

/// 主键字段解析：旧版 DTO 为十进制字符串（DecimalID，避免 JS 精度丢失），兼容数字。
fn id_of(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

/// 活动输入 → 领域校验结构（时间文本原样交由 normalize 解析 RFC3339）。
fn activity_input(v: &Value) -> ActivityRuleInput {
    ActivityRuleInput {
        name: v["name"].as_str().unwrap_or_default().to_string(),
        trigger_type: v["trigger_type"].as_str().unwrap_or_default().to_string(),
        coupon_id: id_of(&v["coupon_id"]),
        inviter_coupon_id: v.get("inviter_coupon_id").map(id_of),
        threshold_cents: v["threshold_cents"].as_i64().unwrap_or(0),
        max_grants: v["max_grants"].as_i64().unwrap_or(0) as i32,
        per_user_limit: v["per_user_limit"].as_i64().unwrap_or(0) as i32,
        status: v["status"].as_str().unwrap_or_default().to_string(),
        start_at: v["start_at"].as_str().unwrap_or_default().to_string(),
        end_at: v["end_at"].as_str().unwrap_or_default().to_string(),
    }
}

async fn create_activity(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.activity.manage", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "活动规则无效", &rid);
    };
    let ip = real_ip(&headers);
    match create_activity_rule(&deps.db, &guard.profile, &ip, &rid, activity_input(&v)).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => coupon_error(e, &rid),
    }
}

async fn update_activity(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "coupon.activity.manage", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, Code::BadRequest as i64, "活动规则无效", &rid);
    };
    let ip = real_ip(&headers);
    match update_activity_rule(&deps.db, &guard.profile, &ip, &rid, id, activity_input(&v)).await {
        Ok(()) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => coupon_error(e, &rid),
    }
}
