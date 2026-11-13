//! 后台会话服务编排：登录（密码 → MFA）→ 签发；刷新；注销；实时鉴权。
//! JWT（admin kind，8 小时）内带 sid，实时权限每次请求从库读取（不信任令牌内旧角色）。

use super::repo;
use super::session;
use super::{AccountIdentity, OpsError, Profile};
use sqlx::MySqlPool;

pub const ACCESS_TTL_SECS: u64 = chargepilot_platform::jwt::ADMIN_TTL_SECS;

/// 登录结果：MFA 账号返回挑战，非 MFA 账号返回完整回执。
pub enum LoginOutcome {
    Challenge(String),
    Access(AccessReply),
}

/// 后台令牌响应（JSON 平铺：Profile 字段 + 令牌字段，键名与旧版一致）。
#[derive(Debug, serde::Serialize)]
pub struct AccessReply {
    #[serde(flatten)]
    pub profile: Profile,
    pub token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

pub struct SessionService<'a> {
    pub db: &'a MySqlPool,
    pub redis_url: &'a str,
    pub jwt_secret: &'a str,
}

impl<'a> SessionService<'a> {
    /// 登录：密码校验（含锁定/审计）→ MFA 挑战或直接签发。
    /// IP 频次检查由 HTTP 层负责（每请求恰好一次，避免双计数）。
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        ip: &str,
    ) -> Result<LoginOutcome, OpsError> {
        let account = repo::verify_login(self.db, username, password, ip).await?;
        let identity = AccountIdentity {
            id: account.id,
            auth_version: account.auth_version,
            username: account.username.clone(),
        };
        if account.mfa_enabled {
            let challenge = session::begin_mfa(self.redis_url, &identity)
                .await
                .map_err(OpsError::from)?;
            return Ok(LoginOutcome::Challenge(challenge));
        }
        Ok(LoginOutcome::Access(self.complete_login(&account).await?))
    }

    /// MFA 完成：读挑战 → 校验验证码（防重放）→ 原子消费 → 签发。
    pub async fn verify_mfa(
        &self,
        challenge_token: &str,
        code: &str,
        ip: &str,
    ) -> Result<AccessReply, OpsError> {
        let identity = session::resolve_mfa(self.redis_url, challenge_token).await?;
        let account = repo::find_by_username(self.db, &identity.username)
            .await
            .map_err(OpsError::from)?
            .ok_or(OpsError::Credentials)?;
        repo::verify_mfa(self.db, &account, code, ip).await?;
        if !session::consume_mfa(self.redis_url, challenge_token, &identity)
            .await
            .map_err(OpsError::from)?
        {
            return Err(OpsError::InvalidChallenge);
        }
        self.complete_login(&account).await
    }

    /// 刷新：轮换 → 实时资料 + 认证版本核对 → 重新签发；失败尽力撤销新令牌。
    pub async fn refresh(&self, token: &str) -> Result<AccessReply, OpsError> {
        let (identity, new_refresh) = session::rotate(self.redis_url, token).await?;
        let sid = new_refresh
            .strip_prefix(session::ADMIN_TOKEN_LAYOUT.token_prefix)
            .and_then(|r| r.split_once('.'))
            .map(|(sid, _)| sid.to_string())
            .unwrap_or_default();
        let issued = self
            .issue_with_refresh(&identity, &sid, new_refresh.clone())
            .await;
        if issued.is_err() {
            let _ = session::revoke(self.redis_url, &new_refresh).await;
        }
        issued
    }

    /// 注销。
    pub async fn logout(&self, token: &str) -> Result<(), OpsError> {
        session::revoke(self.redis_url, token)
            .await
            .map_err(OpsError::from)?;
        Ok(())
    }

    /// 实时鉴权：JWT → 会话记录（含认证版本）→ 与库中资料版本比对 → 实时权限。
    /// 返回 (资料, sid)；权限检查由路由层用 profile.permissions 做。
    pub async fn authenticate(&self, token: &str) -> Result<(Profile, String), OpsError> {
        let claims = chargepilot_platform::jwt::verify(
            self.jwt_secret,
            chargepilot_platform::jwt::Kind::Admin,
            token,
        )
        .map_err(|_| OpsError::Credentials)?;
        if claims.sub == 0 {
            return Err(OpsError::Credentials);
        }
        let in_session = session::find(self.redis_url, &claims.sid)
            .await
            .map_err(OpsError::from)?
            .ok_or(OpsError::Credentials)?;
        if in_session.id != claims.sub {
            return Err(OpsError::Credentials);
        }
        let profile = repo::load_profile(self.db, claims.sub)
            .await
            .map_err(OpsError::from)?;
        if profile.auth_version != in_session.auth_version {
            return Err(OpsError::Credentials);
        }
        Ok((profile, claims.sid))
    }

    async fn complete_login(&self, account: &super::AdminAccount) -> Result<AccessReply, OpsError> {
        let identity = AccountIdentity {
            id: account.id,
            auth_version: account.auth_version,
            username: account.username.clone(),
        };
        let refresh = session::create(self.redis_url, &identity)
            .await
            .map_err(OpsError::from)?;
        let sid = refresh
            .strip_prefix(session::ADMIN_TOKEN_LAYOUT.token_prefix)
            .and_then(|r| r.split_once('.'))
            .map(|(sid, _)| sid.to_string())
            .unwrap_or_default();
        self.issue_with_refresh(&identity, &sid, refresh).await
    }

    async fn issue_with_refresh(
        &self,
        identity: &AccountIdentity,
        sid: &str,
        refresh: String,
    ) -> Result<AccessReply, OpsError> {
        let profile = repo::load_profile(self.db, identity.id).await?;
        if profile.auth_version != identity.auth_version {
            return Err(OpsError::Credentials);
        }
        let token = chargepilot_platform::jwt::sign_with_sid(
            self.jwt_secret,
            chargepilot_platform::jwt::Kind::Admin,
            identity.id,
            sid.to_string(),
        )
        .map_err(OpsError::from)?;
        Ok(AccessReply {
            profile,
            token: token.clone(),
            access_token: token,
            refresh_token: refresh,
            expires_in: ACCESS_TTL_SECS,
        })
    }
}

/// 按 ID 取账号最小身份（实时鉴权的版本核对）。
async fn find_identity_by_id(db: &MySqlPool, id: u64) -> Result<AccountIdentity, OpsError> {
    let row: Option<(u64, u64, String)> = sqlx::query_as(
        "SELECT `id`, `auth_version`, `username` FROM `admin_user` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map_err(OpsError::from)?;
    row.map(|(id, auth_version, username)| AccountIdentity {
        id,
        auth_version,
        username,
    })
    .ok_or(OpsError::Credentials)
}
