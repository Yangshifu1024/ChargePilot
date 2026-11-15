//! 卡域后台门面（票 19 热区批：原 admin_users_cards.rs 的在线卡切片）。
//!
//! 在线卡列表要求 user.read；卡绑定与状态变更要求 online_card.manage。路由层
//! 只做参数解析与状态码映射，角色与权限的最终判定在各服务函数内复核
//! （与旧版 actor 实时权限语义一致）。

use crate::errormap::ops_error;
use crate::errormap::{card_error, rid_of};
use crate::guard::AdminGuard;
use crate::guard::deny;
use crate::state::ApiDeps;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn bad_request(message: &str, rid: &str) -> Response {
    ops_error(OpsError::Input(message.to_string()), rid)
}

/// 路径 ID 必须为正整数（旧版 AdminPathID）。
fn path_id(raw: &str, rid: &str) -> Result<u64, Response> {
    raw.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| bad_request("ID 必须为正整数", rid))
}

#[derive(Deserialize)]
struct BindBody {
    card_no: String,
    /// 完整用户主键，大整数由十进制字符串保留精度（旧版 DecimalID）。
    user_id: String,
    /// 人工核验依据。
    reason: String,
}

const BIND_INPUT_MESSAGE: &str = "请填写32位十进制卡号、用户及核验绑定依据";

async fn list_online_cards(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "user.read", &rid) {
        return resp;
    }
    match chargepilot_mod_card::admin::list_cards(&deps.db, &guard.profile).await {
        Ok(items) => httpx::ok(
            json!({"items": items, "permissions": guard.profile.permissions}),
            &rid,
        ),
        Err(e) => card_error(e, "卡状态与依据无效", &rid),
    }
}

async fn bind_online_card(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    axum::Json(body): axum::Json<BindBody>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "online_card.manage", &rid) {
        return resp;
    }
    // 旧版 DecimalID 解析失败即输入错误，文案与业务输入校验一致。
    let Ok(user_id) = body.user_id.parse::<u64>() else {
        return bad_request(BIND_INPUT_MESSAGE, &rid);
    };
    match chargepilot_mod_card::admin::bind_card(
        &deps.db,
        &guard.profile,
        &body.card_no,
        user_id,
        &body.reason,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => card_error(e, BIND_INPUT_MESSAGE, &rid),
    }
}

#[derive(Deserialize)]
struct CardStatusBody {
    /// online_card.status：active/lost/disabled/unbound。
    status: String,
    /// 人工依据。
    reason: String,
}

async fn online_card_status(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<CardStatusBody>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "online_card.manage", &rid) {
        return resp;
    }
    let card_id = match path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match chargepilot_mod_card::admin::change_card_status(
        &deps.db,
        &guard.profile,
        card_id,
        &body.status,
        &body.reason,
    )
    .await
    {
        Ok(()) => httpx::ok(json!({"id": card_id}), &rid),
        Err(e) => card_error(e, "卡状态与依据无效", &rid),
    }
}
/// 在线卡路由（票 19 热区批）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/online-cards",
            axum::routing::get(list_online_cards),
        )
        .route(
            "/api/v1/admin/online-cards/bind",
            axum::routing::post(bind_online_card),
        )
        .route(
            "/api/v1/admin/online-cards/{id}/status",
            axum::routing::post(online_card_status),
        )
}
