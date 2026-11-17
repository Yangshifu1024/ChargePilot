//! 身份域用户端门面：登录/刷新/登出、资料、手机号绑定。
//! 路径与响应形状为前端契约：`POST …/auth/login|refresh|logout`，`GET …/profile`。

use crate::errormap::rid_of;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use crate::{bearer, identity_error, phone_error, respond};
use axum::Json as AxumJson;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_identity::IdentityError;
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
        // 手机号绑定/解绑。
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

// ---- 手机号绑定 ----
// development 模式可提交明文号码；wechat 模式仅收授权 code，交换未接入前返回「暂不可用」。

/// 手机号域错误 → HTTP（固定业务码与文案）。
#[derive(Deserialize)]
struct PhoneBody {
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// 绑定手机号：成功只返回掩码。
async fn phone_bind(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    body: Option<AxumJson<PhoneBody>>,
) -> Response {
    let rid = rid_of(&headers);
    // 严格 JSON 输入：缺失或类型不符按请求无效处理。
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

/// 解除手机号绑定。
async fn phone_unbind(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
) -> Response {
    let rid = rid_of(&headers);
    match phone::unbind(&deps.db, uid).await {
        Ok(()) => httpx::ok(json!({"bound": false, "unbound": true}), &rid),
        Err(e) => phone_error(e, &rid),
    }
}
