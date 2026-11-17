//! 营销域用户端门面。

use crate::errormap::{coupon_query_error, rid_of};
use crate::params::UserPaging;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_marketing::query::{available, mine};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/coupon/my", axum::routing::get(my_coupons))
        .route("/api/v1/user/coupons", axum::routing::get(list_available))
}

/// 券视图错误 → HTTP。
#[derive(Deserialize)]
struct MyCouponQuery {
    status: Option<String>,
    only_usable: Option<String>,
}

async fn my_coupons(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
    Query(q): Query<MyCouponQuery>,
) -> Response {
    let rid = rid_of(&headers);
    match mine(
        &deps.db,
        uid,
        page.into(),
        page_size.into(),
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
        Err(e) => coupon_query_error(e, &rid),
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
        Err(e) => coupon_query_error(e, &rid),
    }
}
