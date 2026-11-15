//! 运营域用户端门面（票 18 冷区批：原 user_ops.rs 归位）。

use crate::params::UserPaging;
use crate::state::ApiDeps;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route("/api/v1/user/announcement/list", axum::routing::get(list))
}

async fn list(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    match chargepilot_mod_ops::announcement::public_list(&deps.db, page, page_size).await {
        Ok((items, page, page_size)) => httpx::ok(
            json!({"items": items, "page": page, "page_size": page_size}),
            &rid,
        ),
        Err(_) => crate::respond(
            503,
            Code::ServiceUnavailable as i64,
            "公告暂时无法读取",
            &rid,
        ),
    }
}
