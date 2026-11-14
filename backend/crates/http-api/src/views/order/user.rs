//! 订单域用户端门面（票 19 热区批：原 user_identity_cards.rs 的报修/评价提交切片）。

//! C 端身份与在线卡路由：手机号绑定/解绑、在线卡列表/状态变更、订单反馈提交。
//! 路径、码值与文案逐字对齐旧版（internal/http/user/identity/phone.go、
//! card/api.go、order/feedback.go）。

use crate::state::{ApiDeps, UserSessionBase};
use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_order::feedback::{FeedbackSubmitError, FeedbackSubmitInput};
use chargepilot_platform::httpx;
use serde_json::json;
use std::sync::Arc;

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

async fn feedback_submit(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Path(order_id): Path<String>,
    body: Option<AxumJson<FeedbackSubmitInput>>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let oid =
        match chargepilot_mod_order::feedback::resolve_reference(&deps.db, uid, &order_id).await {
            Ok(id) => id,
            Err(FeedbackSubmitError::Input(msg)) => return crate::respond(400, 1005, &msg, &rid),
            Err(FeedbackSubmitError::NotFound) => {
                return crate::respond(404, 1004, "订单不存在", &rid);
            }
            Err(e) => {
                tracing::warn!(error = %e, "订单引用解析失败");
                return crate::respond(503, 5003, "数据暂时无法保存，请稍后重试", &rid);
            }
        };
    let Some(AxumJson(input)) = body else {
        return crate::respond(400, 1005, "反馈请求无效", &rid);
    };
    match chargepilot_mod_order::feedback::submit_feedback(&deps.db, uid, oid, &input).await {
        Ok(feedback_id) => httpx::ok(json!({"submitted": true, "feedback_id": feedback_id}), &rid),
        Err(FeedbackSubmitError::Input(msg)) => crate::respond(400, 1005, &msg, &rid),
        Err(FeedbackSubmitError::OrderNotReady) => {
            crate::respond(404, 1004, "订单不存在或不可评价", &rid)
        }
        Err(FeedbackSubmitError::Exists) => crate::respond(409, 2009, "该订单已评价", &rid),
        // 业务单号解析失败已在 resolve 阶段返回；提交阶段不再出现。
        Err(FeedbackSubmitError::NotFound) => crate::respond(404, 1004, "订单不存在", &rid),
        Err(e) => {
            tracing::warn!(error = %e, "反馈提交失败");
            crate::respond(503, 5003, "反馈提交失败，请稍后重试", &rid)
        }
    }
}
/// 报修/评价提交路由（票 19 热区批；路径为原 /user/feedback）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/user/charge/{order_id}/feedback",
        axum::routing::post(feedback_submit),
    )
}
