//! 卡域用户端门面。
//!
//! 在线卡 C 端仅可写 lost/unbound。

use crate::errormap::rid_of;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

async fn cards(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_card::user_card::list(&deps.db, uid).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => {
            tracing::warn!(error = %e, "用户卡列表失败");
            crate::respond(503, Code::DependencyFailed as i64, "卡信息暂不可用", &rid)
        }
    }
}

#[derive(Deserialize)]
struct CardStatusBody {
    #[serde(default)]
    status: Option<String>,
}

/// 挂失或解绑本人卡。
async fn card_status(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    Path(card_no): Path<String>,
    body: Option<AxumJson<CardStatusBody>>,
) -> Response {
    let rid = rid_of(&headers);
    let status = body.and_then(|b| b.0.status).unwrap_or_default();
    match chargepilot_mod_card::user_card::change_status(&deps.db, uid, &card_no, &status).await {
        Ok(()) => httpx::ok(json!({"status": status}), &rid),
        Err(chargepilot_mod_card::UserCardError::Input) => {
            crate::respond(400, Code::BadRequest as i64, "仅可挂失或解绑自己的卡", &rid)
        }
        Err(chargepilot_mod_card::UserCardError::NotFound) => {
            crate::respond(404, Code::NotFound as i64, "卡不存在", &rid)
        }
        Err(e) => {
            tracing::warn!(error = %e, "用户卡状态变更失败");
            crate::respond(503, Code::DependencyFailed as i64, "卡状态暂不可保存", &rid)
        }
    }
}

/// 订单反馈提交：路径参数先解析（数字主键或业务单号），再绑定 JSON
/// （FeedbackAPI.submitFeedback：业务单号先查再绑定 JSON，数字主键
/// 用户卡片路由。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/cards", axum::routing::get(cards))
        .route(
            "/api/v1/user/cards/{card_no}/status",
            axum::routing::post(card_status),
        )
}
