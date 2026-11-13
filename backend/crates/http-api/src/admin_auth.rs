//! 后台身份路由（路径/码值/文案对齐旧版）：
//! POST login | mfa | refresh | logout，GET me。实时鉴权由 `admin_guard` 提供。

use crate::deps::ApiDeps;
use crate::{bearer, respond};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::service::{AccessReply, LoginOutcome, SessionService};
use chargepilot_platform::httpx;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/auth/login", axum::routing::post(login))
        .route("/api/v1/admin/auth/mfa", axum::routing::post(verify_mfa))
        .route("/api/v1/admin/auth/refresh", axum::routing::post(refresh))
        .route("/api/v1/admin/auth/logout", axum::routing::post(logout))
        .route("/api/v1/admin/auth/me", axum::routing::get(me))
}

fn service(deps: &Arc<ApiDeps>) -> SessionService<'_> {
    SessionService {
        db: &deps.db,
        redis_url: &deps.redis_url,
        jwt_secret: &deps.jwt_secret,
    }
}

/// OpsError → HTTP（码值与文案沿用旧版字面量契约）。
pub fn ops_error(err: OpsError, request_id: &str) -> Response {
    match &err {
        OpsError::Credentials => respond(401, 1001, "用户名或密码错误，或登录已失效", request_id),
        OpsError::Locked(msg) => respond(423, 2006, msg, request_id),
        OpsError::UsedTotp => respond(
            401,
            1001,
            "该验证码已被使用，请等待下一个验证码",
            request_id,
        ),
        OpsError::InvalidTotp => respond(401, 1001, "验证码无效或已过期", request_id),
        OpsError::InvalidChallenge => respond(401, 1001, "登录已失效，请重新登录", request_id),
        OpsError::LoginRate => {
            let mut resp = respond(429, 4291, "登录尝试过于频繁，请稍后再试", request_id);
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("60"));
            resp
        }
        OpsError::Permission => respond(403, 1003, "无权执行该操作", request_id),
        OpsError::Storage(_) => {
            tracing::warn!(cause = %err, "后台登录存储失败");
            respond(503, 5003, "登录服务暂不可用", request_id)
        }
    }
}

fn reply_response(reply: AccessReply, rid: &str) -> Response {
    match serde_json::to_value(&reply) {
        Ok(data) => httpx::ok(data, rid),
        Err(_) => respond(503, 5003, "登录服务暂不可用", rid),
    }
}

fn client_ip(headers: &HeaderMap) -> String {
    // 单体直连场景：优先取直连地址；转发头在阶段 09 与外部 LB 一起定
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

async fn login(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let ip = client_ip(&headers);
    if let Err(e) = chargepilot_mod_ops::session::check_rate(&deps.redis_url, &ip).await {
        return ops_error(e, &rid);
    }
    let (username, password) = match body.as_ref().map(|b| &b.0) {
        Some(v) => (
            v["username"].as_str().unwrap_or_default().to_string(),
            v["password"].as_str().unwrap_or_default().to_string(),
        ),
        None => (String::new(), String::new()),
    };
    if username.is_empty() || password.is_empty() || password.len() > 72 {
        return respond(400, 1005, "请输入用户名和密码", &rid);
    }
    match service(&deps).login(&username, &password, &ip).await {
        Ok(LoginOutcome::Challenge(challenge)) => {
            let data = serde_json::json!({ "challenge": challenge });
            httpx::ok(data, &rid)
        }
        Ok(LoginOutcome::Access(reply)) => reply_response(reply, &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn verify_mfa(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let ip = client_ip(&headers);
    let (challenge, code) = match body.as_ref().map(|b| &b.0) {
        Some(v) => (
            v["challenge"].as_str().unwrap_or_default().to_string(),
            v["code"].as_str().unwrap_or_default().to_string(),
        ),
        None => (String::new(), String::new()),
    };
    if challenge.is_empty() || code.is_empty() || code.len() > 16 {
        return respond(400, 1005, "请提供登录挑战与验证码", &rid);
    }
    match service(&deps).verify_mfa(&challenge, &code, &ip).await {
        Ok(reply) => reply_response(reply, &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn refresh(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(401, 1001, "用户名或密码错误，或登录已失效", &rid);
    };
    match service(&deps).refresh(token).await {
        Ok(reply) => reply_response(reply, &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn logout(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(401, 1001, "用户名或密码错误，或登录已失效", &rid);
    };
    match service(&deps).logout(token).await {
        Ok(()) => httpx::ok(serde_json::json!({ "revoked": true }), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

/// 实时鉴权 + 资料回显。
async fn me(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(401, 1001, "用户名或密码错误，或登录已失效", &rid);
    };
    match service(&deps).authenticate(token).await {
        Ok((profile, _sid)) => {
            let data = serde_json::to_value(&profile).unwrap_or(serde_json::Value::Null);
            httpx::ok(data, &rid)
        }
        Err(e) => ops_error(e, &rid),
    }
}
