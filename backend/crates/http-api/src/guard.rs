//! 后台实时鉴权 extractor：JWT → 会话存在 → 实时 Profile（权限不足由 handler 用 `has()` 判定）。

use crate::bearer;
use crate::errormap::ops_error;
use crate::state::ApiDeps;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use std::sync::Arc;

/// 已通过实时鉴权的管理员（profile 每请求从库读取，权限不信任令牌）。
/// `sid` 是当前会话 ID：本人 MFA 设置用它绑定发起会话（旧版 admin_sid 语义）。
pub struct AdminGuard {
    pub profile: Profile,
    pub sid: String,
}

impl AdminGuard {
    pub fn has(&self, permission: &str) -> bool {
        self.profile.permissions.iter().any(|p| p == permission)
    }
}

impl axum::extract::FromRequestParts<Arc<ApiDeps>> for AdminGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<ApiDeps>,
    ) -> Result<Self, Self::Rejection> {
        let rid = chargepilot_platform::httpx::request_id_from(&parts.headers);
        let Some(token) = bearer(&parts.headers) else {
            return Err(ops_error(OpsError::Credentials, &rid));
        };
        let svc = chargepilot_mod_ops::service::SessionService {
            db: &state.db,
            redis_url: &state.redis_url,
            jwt_secret: &state.jwt_secret,
        };
        match svc.authenticate(token).await {
            Ok((profile, sid)) => Ok(AdminGuard { profile, sid }),
            Err(e) => Err(ops_error(e, &rid)),
        }
    }
}

use chargepilot_mod_ops::Profile;
