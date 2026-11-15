//! 营销域用户端门面（票 18 冷区批：原 user_marketing.rs 归位）。

use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_marketing::CouponQueryError;
use chargepilot_mod_marketing::query::{available, mine};
use chargepilot_platform::error::Code;
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
        CouponQueryError::Input(m) => crate::respond(400, Code::BadRequest as i64, m, rid),
        CouponQueryError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券查询处理失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "优惠券暂时无法读取",
                rid,
            )
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
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    Query(q): Query<MyCouponQuery>,
) -> Response {
    let rid = rid_of(&headers);
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
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
) -> Response {
    let rid = rid_of(&headers);
    match available(&deps.db, uid).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => query_error(e, &rid),
    }
}
