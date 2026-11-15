//! 身份域用户端门面（票 19 热区批：原 user_auth.rs 归位；phone 切片随本票并入）。

//! C 端身份路由（路径与响应形状为前端契约，逐字节对齐旧版）：
//! POST /api/v1/user/auth/login | refresh | logout，GET /api/v1/user/profile。

use crate::state::{ApiDeps, UserSessionBase};
use crate::{bearer, identity_error, respond};
use axum::Json as AxumJson;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_identity::IdentityError;
use chargepilot_mod_identity::PhoneError;
use chargepilot_mod_identity::phone::{self, PhoneInput};
use chargepilot_mod_identity::service::SessionService;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx::{self, Envelope};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/auth/login", axum::routing::post(login))
        .route("/api/v1/user/auth/refresh", axum::routing::post(refresh))
        .route("/api/v1/user/auth/logout", axum::routing::post(logout))
        .route("/api/v1/user/profile", axum::routing::get(profile))
        // 手机号绑定/解绑（票 19：原 user_identity_cards 的 phone 切片归位）。
        .route("/api/v1/user/phone/bind", axum::routing::post(phone_bind))
        .route(
            "/api/v1/user/phone/unbind",
            axum::routing::post(phone_unbind),
        )
}

fn service(deps: &Arc<ApiDeps>) -> SessionService<'_> {
    SessionService {
        db: &deps.db,
        redis_url: &deps.redis_url,
        jwt_secret: &deps.jwt_secret,
        allocator: &deps.allocator,
        exchanger: &deps.exchanger,
    }
}

async fn login(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let code = body
        .as_ref()
        .and_then(|b| b.0.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if code.is_empty() {
        return respond(400, Code::BadRequest as i64, "code is required", &rid);
    }
    match service(&deps).login(&code).await {
        Ok(result) => {
            let data = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
            httpx::ok(data, &rid)
        }
        Err(e) => identity_error(e, &rid),
    }
}

async fn refresh(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(
            401,
            Code::Unauthorized as i64,
            "refresh token invalid",
            &rid,
        );
    };
    match service(&deps).refresh(token).await {
        Ok(result) => {
            let data = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
            httpx::ok(data, &rid)
        }
        Err(e) => identity_error(e, &rid),
    }
}

async fn logout(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(
            401,
            Code::Unauthorized as i64,
            "refresh token invalid",
            &rid,
        );
    };
    match service(&deps).logout(token).await {
        Ok(()) => httpx::ok(serde_json::Value::Null, &rid),
        Err(e) => identity_error(e, &rid),
    }
}

/// 实时鉴权 extractor 的手动版本：校验通过后组装 profile。
async fn profile(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(401, Code::Unauthorized as i64, "access token invalid", &rid);
    };
    let svc = service(&deps);
    match svc.authenticate(token).await {
        Ok(user_id) => match svc.profile(user_id).await {
            Ok(profile) => {
                let data = serde_json::to_value(&profile).unwrap_or(serde_json::Value::Null);
                httpx::ok(data, &rid)
            }
            Err(e) => identity_error(e, &rid),
        },
        Err(IdentityError::TokenInvalid) => {
            respond(401, Code::Unauthorized as i64, "access token invalid", &rid)
        }
        Err(IdentityError::SessionExpired) => {
            respond(401, Code::Unauthorized as i64, "session expired", &rid)
        }
        Err(e) => identity_error(e, &rid),
    }
}

// 信封类型在 404/health 之外的场景复用（供编译期形状检查）
#[allow(dead_code)]
type _EnvelopeCheck = Envelope<serde_json::Value>;

// ---- 手机号绑定（票 19：原 user_identity_cards.rs 的 phone 切片）----

// C 端身份与在线卡路由：手机号绑定/解绑、在线卡列表/状态变更、订单反馈提交。
// 路径、码值与文案逐字对齐旧版（internal/http/user/identity/phone.go、
// card/api.go、order/feedback.go）。
//
// 挂载行：`crates/http-api/src/lib.rs` 的 build_router 中追加
// `.merge(user_identity_cards::router())`（本文件不改 lib.rs，由协调者统一挂载）。
//
// 手机号绑定按登录模式分流（deps.exchanger）：development 接受明文 phone；
// wechat 仅收授权 code，正式号码交换能力接入前统一按「暂不可用」拒绝
// （旧版 ErrPhoneUnavailable 语义）。在线卡状态 C 端仅可写 lost/unbound。

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// 手机号域错误 → HTTP（码值与文案沿用旧版字面量契约）。
fn phone_error(e: PhoneError, rid: &str) -> Response {
    match &e {
        PhoneError::Authorization => crate::respond(
            400,
            Code::BadRequest as i64,
            "请使用微信手机号授权凭证",
            rid,
        ),
        PhoneError::Unavailable => crate::respond(
            503,
            Code::ServiceUnavailable as i64,
            "手机号授权暂不可用",
            rid,
        ),
        PhoneError::Exchange => crate::respond(
            400,
            Code::NotFound as i64,
            "手机号授权失败，请重新授权",
            rid,
        ),
        PhoneError::Input => crate::respond(
            400,
            Code::BadRequest as i64,
            "请输入有效的中国大陆手机号",
            rid,
        ),
        PhoneError::Taken => {
            crate::respond(409, Code::Conflict as i64, "该手机号已绑定其他账号", rid)
        }
        PhoneError::Storage(_) => {
            tracing::warn!(error = %e, "手机号保存失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法保存，请稍后重试",
                rid,
            )
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
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    body: Option<AxumJson<PhoneBody>>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    // 严格 JSON 输入：缺失或类型不符按请求无效处理（旧版 ShouldBindJSON）。
    let Some(AxumJson(body)) = body else {
        return crate::respond(400, Code::BadRequest as i64, "手机号请求无效", &rid);
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
async fn phone_unbind(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match phone::unbind(&deps.db, uid).await {
        Ok(()) => httpx::ok(json!({"bound": false, "unbound": true}), &rid),
        Err(e) => phone_error(e, &rid),
    }
}
