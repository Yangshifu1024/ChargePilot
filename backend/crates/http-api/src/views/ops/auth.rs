//! 运营域后台门面·认证（票 18 冷区批：原 admin_auth.rs 归位）。

use crate::errormap::{bearer, ops_error, respond};
use crate::state::ApiDeps;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::service::{AccessReply, LoginOutcome, SessionService};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use std::sync::Arc;

pub fn auth_router() -> axum::Router<Arc<ApiDeps>> {
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
fn reply_response(reply: AccessReply, rid: &str) -> Response {
    match serde_json::to_value(&reply) {
        Ok(data) => httpx::ok(data, rid),
        Err(_) => respond(
            503,
            Code::ServiceUnavailable as i64,
            "登录服务暂不可用",
            rid,
        ),
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
        return respond(400, Code::BadRequest as i64, "请输入用户名和密码", &rid);
    }
    match service(&deps).login(&username, &password, &ip).await {
        Ok(LoginOutcome::Challenge(challenge)) => {
            // 前端契约（Login.tsx / 旧版语义）：密码通过待验码时回
            // mfa_required + mfa_challenge，前端以 mfa_challenge 提交第二步。
            let data = serde_json::json!({ "mfa_required": true, "mfa_challenge": challenge });
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
            v["mfa_challenge"].as_str().unwrap_or_default().to_string(),
            v["code"].as_str().unwrap_or_default().to_string(),
        ),
        None => (String::new(), String::new()),
    };
    if challenge.is_empty() || code.is_empty() || code.len() > 16 {
        return respond(400, Code::BadRequest as i64, "请提供登录挑战与验证码", &rid);
    }
    match service(&deps).verify_mfa(&challenge, &code, &ip).await {
        Ok(reply) => reply_response(reply, &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn refresh(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            &rid,
        );
    };
    match service(&deps).refresh(token).await {
        Ok(reply) => reply_response(reply, &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn logout(State(deps): State<Arc<ApiDeps>>, headers: HeaderMap) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(token) = bearer(&headers) else {
        return respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            &rid,
        );
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
        return respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            &rid,
        );
    };
    match service(&deps).authenticate(token).await {
        Ok((profile, _sid)) => {
            let data = serde_json::to_value(&profile).unwrap_or(serde_json::Value::Null);
            httpx::ok(data, &rid)
        }
        Err(e) => ops_error(e, &rid),
    }
}
