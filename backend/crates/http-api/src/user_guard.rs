//! 用户端会话提取横切（票 17）：从业务文件归入鉴权模块。
//!
//! user_id_from 供各用户域路由复用同一鉴权路径（旧版同用 user 组鉴权）；
//! 失败映射经 errormap 的身份错误直映契约（401/1001 系）。

use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::error::Code;

use crate::errormap::{bearer, respond};
use crate::state::UserSessionBase;

/// 用户会话鉴权：Bearer 令牌 → user_id；失败即 401 信封。
/// 收域组会话基础件（票 17：处理器单 State 提取自足）。
pub async fn user_id_from(base: &UserSessionBase, headers: &HeaderMap) -> Result<u64, Response> {
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
