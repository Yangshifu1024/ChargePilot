//! 订单域用户端门面（票 19 热区批：原 user_identity_cards.rs 的报修/评价提交切片）。

//! C 端身份与在线卡路由：手机号绑定/解绑、在线卡列表/状态变更、订单反馈提交。
//! 路径、码值与文案逐字对齐旧版（internal/http/user/identity/phone.go、
//! card/api.go、order/feedback.go）。

use crate::errormap::rid_of;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_order::feedback::FeedbackSubmitInput;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde_json::json;
use std::sync::Arc;

async fn feedback_submit(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    Path(order_id): Path<String>,
    body: Option<AxumJson<FeedbackSubmitInput>>,
) -> Response {
    let rid = rid_of(&headers);
    let oid =
        match chargepilot_mod_order::feedback::resolve_reference(&deps.db, uid, &order_id).await {
            Ok(id) => id,
            Err(e) => return crate::errormap::feedback_resolve_error(e, &rid),
        };
    let Some(AxumJson(input)) = body else {
        return crate::respond(400, Code::BadRequest as i64, "反馈请求无效", &rid);
    };
    match chargepilot_mod_order::feedback::submit_feedback(&deps.db, uid, oid, &input).await {
        Ok(feedback_id) => httpx::ok(json!({"submitted": true, "feedback_id": feedback_id}), &rid),
        Err(e) => crate::errormap::feedback_submit_error(e, &rid),
    }
}
/// 报修/评价提交路由（票 19 热区批；路径为原 /user/feedback）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/user/charge/{order_id}/feedback",
        axum::routing::post(feedback_submit),
    )
}
