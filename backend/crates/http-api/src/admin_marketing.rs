//! 后台营销路由：优惠券模板、发放统计与人工发券（路径/权限码对齐旧版
//! admin/marketing 装配）。活动规则路由随批 3 追加到本文件。
//! 审计（module/target = coupon）由服务层与业务写入同事务落库。

use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_marketing::coupon::{
    CouponError, CouponRow, CouponUpdate, GrantInput, coupon_stats, create_coupon, grant_coupon,
    list_coupons, update_coupon,
};
use chargepilot_platform::httpx;
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
        Some(crate::respond(403, 1003, "无权执行此操作", rid))
    }
}

/// 券域错误 → HTTP（映射对齐旧版 failure + AdminResourceFailure）。
fn coupon_error(e: CouponError, rid: &str) -> Response {
    match &e {
        CouponError::Input(m) => crate::respond(400, 1005, m, rid),
        CouponError::Forbidden => crate::respond(403, 1003, "无权执行此操作", rid),
        CouponError::Conflict(m) => crate::respond(409, 2009, m, rid),
        CouponError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        CouponError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券请求处理失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
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
        return crate::respond(400, 1005, "优惠券名称、有效期或额度无效", &rid);
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
        return crate::respond(400, 1005, "名称或状态无效", &rid);
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
        return crate::respond(400, 1005, "请求编号或用户编号无效", &rid);
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
