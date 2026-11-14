//! C 端营销路由：本人持券分页与支付前可用券（路径对齐旧版 user/marketing 装配）。
//! 只读视图：到期状态由服务层按当前时间派生展示，不写回数据库。

use crate::deps::{ApiDeps, UserSessionBase};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_marketing::query::{CouponQueryError, available, mine};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/coupon/my", axum::routing::get(my_coupons))
        .route("/api/v1/user/coupons", axum::routing::get(list_available))
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// 券视图错误 → HTTP（旧版 myCoupons 的映射：输入 400，其余 503 固定文案）。
fn query_error(e: CouponQueryError, rid: &str) -> Response {
    match &e {
        CouponQueryError::Input(m) => crate::respond(400, 1005, m, rid),
        CouponQueryError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券查询处理失败");
            crate::respond(503, 5003, "优惠券暂时无法读取", rid)
        }
    }
}

#[derive(Deserialize)]
struct MyCouponQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    only_usable: Option<String>,
}

async fn my_coupons(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<MyCouponQuery>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match mine(
        &deps.db,
        uid,
        q.page.unwrap_or(1).max(1),
        q.page_size.unwrap_or(20).clamp(1, 100),
        q.status.as_deref().unwrap_or(""),
        q.only_usable.as_deref() == Some("true"),
        chrono::Utc::now(),
    )
    .await
    {
        Ok(out) => {
            let body = serde_json::to_value(&out).unwrap_or(json!({}));
            httpx::ok(body, &rid)
        }
        Err(e) => query_error(e, &rid),
    }
}

async fn list_available(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match available(&deps.db, uid).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => query_error(e, &rid),
    }
}
