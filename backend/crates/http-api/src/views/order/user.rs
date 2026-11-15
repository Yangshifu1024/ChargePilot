//! 订单域用户端门面（票 19 热区批：原 user_identity_cards.rs 的报修/评价提交切片）。

//! C 端身份与在线卡路由：手机号绑定/解绑、在线卡列表/状态变更、订单反馈提交。
//! 路径、码值与文案逐字对齐旧版（internal/http/user/identity/phone.go、
//! card/api.go、order/feedback.go）。

use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_order::FeedbackSubmitError;
use chargepilot_mod_order::feedback::FeedbackSubmitInput;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde_json::json;
use std::sync::Arc;

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

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
            Err(FeedbackSubmitError::Input(msg)) => {
                return crate::respond(400, Code::BadRequest as i64, &msg, &rid);
            }
            Err(FeedbackSubmitError::NotFound) => {
                return crate::respond(404, Code::NotFound as i64, "订单不存在", &rid);
            }
            Err(e) => {
                tracing::warn!(error = %e, "订单引用解析失败");
                return crate::respond(
                    503,
                    Code::ServiceUnavailable as i64,
                    "数据暂时无法保存，请稍后重试",
                    &rid,
                );
            }
        };
    let Some(AxumJson(input)) = body else {
        return crate::respond(400, Code::BadRequest as i64, "反馈请求无效", &rid);
    };
    match chargepilot_mod_order::feedback::submit_feedback(&deps.db, uid, oid, &input).await {
        Ok(feedback_id) => httpx::ok(json!({"submitted": true, "feedback_id": feedback_id}), &rid),
        Err(FeedbackSubmitError::Input(msg)) => {
            crate::respond(400, Code::BadRequest as i64, &msg, &rid)
        }
        Err(FeedbackSubmitError::OrderNotReady) => {
            crate::respond(404, Code::NotFound as i64, "订单不存在或不可评价", &rid)
        }
        Err(FeedbackSubmitError::Exists) => {
            crate::respond(409, Code::Conflict as i64, "该订单已评价", &rid)
        }
        // 业务单号解析失败已在 resolve 阶段返回；提交阶段不再出现。
        Err(FeedbackSubmitError::NotFound) => {
            crate::respond(404, Code::NotFound as i64, "订单不存在", &rid)
        }
        Err(e) => {
            tracing::warn!(error = %e, "反馈提交失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "反馈提交失败，请稍后重试",
                &rid,
            )
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
