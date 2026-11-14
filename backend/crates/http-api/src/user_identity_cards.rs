//! C 端身份与在线卡路由：手机号绑定/解绑、在线卡列表/状态变更、订单反馈提交。
//! 路径、码值与文案逐字对齐旧版（internal/http/user/identity/phone.go、
//! card/api.go、order/feedback.go）。
//!
//! 挂载行：`crates/http-api/src/lib.rs` 的 build_router 中追加
//! `.merge(user_identity_cards::router())`（本文件不改 lib.rs，由协调者统一挂载）。
//!
//! 手机号绑定按登录模式分流（deps.exchanger）：development 接受明文 phone；
//! wechat 仅收授权 code，正式号码交换能力接入前统一按「暂不可用」拒绝
//! （旧版 ErrPhoneUnavailable 语义）。在线卡状态 C 端仅可写 lost/unbound。

use crate::deps::ApiDeps;
use axum::Json as AxumJson;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_identity::phone::{self, PhoneError, PhoneInput};
use chargepilot_mod_order::feedback::{FeedbackSubmitError, FeedbackSubmitInput};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/phone/bind", axum::routing::post(phone_bind))
        .route(
            "/api/v1/user/phone/unbind",
            axum::routing::post(phone_unbind),
        )
        .route("/api/v1/user/cards", axum::routing::get(cards))
        .route(
            "/api/v1/user/cards/{card_no}/status",
            axum::routing::post(card_status),
        )
        .route(
            "/api/v1/user/charge/{order_id}/feedback",
            axum::routing::post(feedback_submit),
        )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// 手机号域错误 → HTTP（码值与文案沿用旧版字面量契约）。
fn phone_error(e: PhoneError, rid: &str) -> Response {
    match &e {
        PhoneError::Authorization => crate::respond(400, 1005, "请使用微信手机号授权凭证", rid),
        PhoneError::Unavailable => crate::respond(503, 5003, "手机号授权暂不可用", rid),
        PhoneError::Exchange => crate::respond(400, 1004, "手机号授权失败，请重新授权", rid),
        PhoneError::Input => crate::respond(400, 1005, "请输入有效的中国大陆手机号", rid),
        PhoneError::Taken => crate::respond(409, 2009, "该手机号已绑定其他账号", rid),
        PhoneError::Storage(_) => {
            tracing::warn!(error = %e, "手机号保存失败");
            crate::respond(503, 5003, "数据暂时无法保存，请稍后重试", rid)
        }
    }
}

#[derive(Deserialize)]
struct PhoneBody {
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// 绑定手机号：成功只返回掩码（旧版 PhoneAPI.bind）。
async fn phone_bind(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<AxumJson<PhoneBody>>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    // 严格 JSON 输入：缺失或类型不符按请求无效处理（旧版 ShouldBindJSON）。
    let Some(AxumJson(body)) = body else {
        return crate::respond(400, 1005, "手机号请求无效", &rid);
    };
    let input = PhoneInput {
        phone: body.phone,
        code: body.code,
    };
    match phone::bind(&deps.db, &deps.exchanger, uid, &input).await {
        Ok(result) => httpx::ok(serde_json::to_value(result).unwrap_or(Value::Null), &rid),
        Err(e) => phone_error(e, &rid),
    }
}

/// 解除手机号绑定（旧版 PhoneAPI.unbind；重复解绑同样成功）。
async fn phone_unbind(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match phone::unbind(&deps.db, uid).await {
        Ok(()) => httpx::ok(json!({"bound": false, "unbound": true}), &rid),
        Err(e) => phone_error(e, &rid),
    }
}

/// 返回本人尚未解绑的卡（旧版 card.API.cards）。
async fn cards(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match chargepilot_mod_card::user_card::list(&deps.db, uid).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => {
            tracing::warn!(error = %e, "用户卡列表失败");
            crate::respond(503, 5001, "卡信息暂不可用", &rid)
        }
    }
}

#[derive(Deserialize)]
struct CardStatusBody {
    #[serde(default)]
    status: Option<String>,
}

/// 挂失或解绑本人卡（旧版 card.API.cardStatus；C 端仅可写 lost/unbound）。
async fn card_status(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Path(card_no): Path<String>,
    body: Option<AxumJson<CardStatusBody>>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let status = body.and_then(|b| b.0.status).unwrap_or_default();
    match chargepilot_mod_card::user_card::change_status(&deps.db, uid, &card_no, &status).await {
        Ok(()) => httpx::ok(json!({"status": status}), &rid),
        Err(chargepilot_mod_card::user_card::UserCardError::Input) => {
            crate::respond(400, 1005, "仅可挂失或解绑自己的卡", &rid)
        }
        Err(chargepilot_mod_card::user_card::UserCardError::NotFound) => {
            crate::respond(404, 1004, "卡不存在", &rid)
        }
        Err(e) => {
            tracing::warn!(error = %e, "用户卡状态变更失败");
            crate::respond(503, 5001, "卡状态暂不可保存", &rid)
        }
    }
}

/// 订单反馈提交：路径参数先解析（数字主键或业务单号），再绑定 JSON
/// （旧版 FeedbackAPI.submitFeedback：业务单号先查再绑定 JSON，数字主键
/// 的归属和状态在提交事务中核对）。
async fn feedback_submit(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Path(order_id): Path<String>,
    body: Option<AxumJson<FeedbackSubmitInput>>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
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
