//! 运营域用户端门面（票 18 冷区批：原 user_ops.rs 归位）。

use crate::deps::ApiDeps;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route("/api/v1/user/announcement/list", axum::routing::get(list))
}

#[derive(Deserialize)]
struct Paging {
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn list(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Query(q): Query<Paging>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let page = q.page.unwrap_or(1);
    let size = q.page_size.unwrap_or(20);
    if !(1..=100_000).contains(&page) || !(1..=100).contains(&size) {
        return crate::respond(400, 1005, "分页参数无效", &rid);
    }
    match chargepilot_mod_ops::announcement::public_list(&deps.db, page as u32, size as u32).await {
        Ok((items, page, page_size)) => httpx::ok(
            json!({"items": items, "page": page, "page_size": page_size}),
            &rid,
        ),
        Err(_) => crate::respond(503, 5003, "公告暂时无法读取", &rid),
    }
}
