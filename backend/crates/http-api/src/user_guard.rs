//! 用户端会话提取横切：从业务文件归入鉴权模块。
//!
//! [`UserGuard`] 与 AdminGuard 同构的鉴权提取器：JWT → 会话存在 → user_id，
//! 失败即 401 信封直映契约（401/1001 系）。处理器签名里一个参数完成鉴权，
//! 不再有「提取 + match 展开」的调用点样板。

use axum::extract::FromRef;
use axum::response::Response;
use chargepilot_platform::error::Code;
use std::sync::Arc;

use crate::errormap::{bearer, respond};
use crate::state::{ApiDeps, UserSessionBase};

/// 已通过用户端会话鉴权的用户 ID：Bearer 令牌 → user_id；失败即 401 信封。
/// 会话基础件经 FromRef 自装配根派生。
pub struct UserGuard(pub u64);

impl axum::extract::FromRequestParts<Arc<ApiDeps>> for UserGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<ApiDeps>,
    ) -> Result<Self, Self::Rejection> {
        let base = UserSessionBase::from_ref(state);
        user_id_from(&base, &parts.headers).await.map(Self)
    }
}

/// 用户会话鉴权实现：Bearer 令牌 → user_id；失败映射经 errormap 的身份错误
/// 直映契约（401/1001 系）。
async fn user_id_from(
    base: &UserSessionBase,
    headers: &axum::http::HeaderMap,
) -> Result<u64, Response> {
    let rid = chargepilot_platform::httpx::request_id_from(headers);
    let Some(token) = bearer(headers) else {
        return Err(respond(
            401,
            Code::Unauthorized as i64,
            "access token invalid",
            &rid,
        ));
    };
    let svc = chargepilot_mod_identity::service::SessionService {
        db: &base.db,
        redis_url: &base.redis_url,
        jwt_secret: &base.jwt_secret,
        allocator: &base.allocator,
        exchanger: &base.exchanger,
    };
    match svc.authenticate(token).await {
        Ok(uid) => Ok(uid),
        Err(chargepilot_mod_identity::IdentityError::SessionExpired) => Err(respond(
            401,
            Code::Unauthorized as i64,
            "session expired",
            &rid,
        )),
        Err(_) => Err(respond(
            401,
            Code::Unauthorized as i64,
            "access token invalid",
            &rid,
        )),
    }
}
