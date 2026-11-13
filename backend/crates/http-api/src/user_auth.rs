//! C 端身份路由（路径与响应形状为前端契约，逐字节对齐旧版）：
//! POST /api/v1/user/auth/login | refresh | logout，GET /api/v1/user/profile。

use crate::deps::ApiDeps;
use crate::{bearer, identity_error, respond};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_identity::IdentityError;
use chargepilot_mod_identity::service::SessionService;
use chargepilot_platform::httpx::{self, Envelope};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/auth/login", axum::routing::post(login))
        .route("/api/v1/user/auth/refresh", axum::routing::post(refresh))
        .route("/api/v1/user/auth/logout", axum::routing::post(logout))
        .route("/api/v1/user/profile", axum::routing::get(profile))
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
        return respond(400, 1005, "code is required", &rid);
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
        return respond(401, 1001, "refresh token invalid", &rid);
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
        return respond(401, 1001, "refresh token invalid", &rid);
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
        return respond(401, 1001, "access token invalid", &rid);
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
        Err(IdentityError::TokenInvalid) => respond(401, 1001, "access token invalid", &rid),
        Err(IdentityError::SessionExpired) => respond(401, 1001, "session expired", &rid),
        Err(e) => identity_error(e, &rid),
    }
}

// 信封类型在 404/health 之外的场景复用（供编译期形状检查）
#[allow(dead_code)]
type _EnvelopeCheck = Envelope<serde_json::Value>;
