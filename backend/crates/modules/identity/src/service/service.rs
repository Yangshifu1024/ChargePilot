//! 会话服务编排：登录 / 刷新 / 注销 / 实时鉴权 / profile。
//! 错误到 HTTP 的映射在 http-api（保持旧版码值与文案）。

use crate::repo::{self, user::UserProfile};
use crate::service::session::{self, USER_TOKEN_LAYOUT};
use crate::service::wechat::Exchanger;
use crate::{IdentityError, repo::user::User};
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::MySqlPool;

pub const ACCESS_TTL_SECS: u64 = chargepilot_platform::jwt::USER_TTL_SECS;

/// 登录响应（JSON 平铺，键名与旧版逐字节一致）。
#[derive(Debug, serde::Serialize)]
pub struct LoginResult {
    pub jwt: String,
    /// 同一访问签名的兼容键（前端历史字段）。
    pub token: String,
    pub refresh_token: String,
    pub jwt_expires_in: u64,
    /// 精确十进制字符串，避免数值精度丢失（旧版语义）。
    pub user_id: String,
    pub is_new_user: bool,
}

/// 刷新响应（平铺）。
#[derive(Debug, serde::Serialize)]
pub struct AccessResult {
    pub jwt: String,
    pub token: String,
    pub refresh_token: String,
    pub jwt_expires_in: u64,
}

/// profile 响应（钱包来自 wallet 薄模块只读投影；未用券数在阶段 05 接入前恒为 0）。
#[derive(Debug, serde::Serialize)]
pub struct Profile {
    #[serde(serialize_with = "ser_u64_string")]
    pub user_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    pub phone_bound: bool,
    pub registered_at: String,
    pub wallet: WalletView,
    pub coupon_unused_count: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct WalletView {
    pub status: String,
    pub available_cents: i64,
    pub frozen_cents: i64,
}

fn ser_u64_string<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// 会话服务（无状态，依赖经参数传入，便于测试）。
pub struct SessionService<'a> {
    pub db: &'a MySqlPool,
    pub redis_url: &'a str,
    pub jwt_secret: &'a str,
    pub allocator: &'a SegmentAllocator,
    pub exchanger: &'a Exchanger,
}

impl<'a> SessionService<'a> {
    /// 登录：换身份 → 建户/恢复 → 建会话 → 签访问令牌。
    pub async fn login(&self, code: &str) -> Result<LoginResult, IdentityError> {
        let identity = self
            .exchanger
            .exchange(code)
            .await
            .map_err(IdentityError::ExchangeFailed)?;
        let (user, is_new) = repo::login_or_create(
            self.db,
            &identity.openid,
            identity.unionid.as_deref(),
            self.allocator,
        )
        .await
        .map_err(IdentityError::from)?;
        let refresh_token = session::create(self.redis_url, user.id, &user.openid)
            .await
            .map_err(IdentityError::from)?;
        let jwt = sign_access(self.jwt_secret, &user, refresh_token_part(&refresh_token))
            .map_err(IdentityError::Storage)?;
        Ok(LoginResult {
            token: jwt.clone(),
            jwt,
            refresh_token,
            jwt_expires_in: ACCESS_TTL_SECS,
            user_id: user.id.to_string(),
            is_new_user: is_new,
        })
    }

    /// 刷新：原子轮换 → 实时状态校验 → 重新签发。冻结则尽力撤销新会话。
    pub async fn refresh(&self, token: &str) -> Result<AccessResult, IdentityError> {
        let rotated = session::rotate(self.redis_url, token).await?;
        if !repo::is_active(self.db, rotated.uid)
            .await
            .map_err(IdentityError::from)?
        {
            let _ = session::revoke(self.redis_url, &rotated.refresh_token).await;
            return Err(IdentityError::AccountFrozen);
        }
        let user = User {
            id: rotated.uid,
            openid: rotated.openid,
            nickname: None,
            avatar_url: None,
            phone: None,
            status: chargepilot_domain::identity::UserStatus::Active,
        };
        let jwt = sign_access(
            self.jwt_secret,
            &user,
            refresh_token_part(&rotated.refresh_token),
        )
        .map_err(IdentityError::Storage)?;
        Ok(AccessResult {
            token: jwt.clone(),
            jwt,
            refresh_token: rotated.refresh_token,
            jwt_expires_in: ACCESS_TTL_SECS,
        })
    }

    /// 注销：按当前或历史摘要撤销；格式错误的令牌静默成功（幂等语义）。
    pub async fn logout(&self, token: &str) -> Result<(), IdentityError> {
        session::revoke(self.redis_url, token)
            .await
            .map_err(IdentityError::from)?;
        Ok(())
    }

    /// 实时鉴权：校验 JWT → 校验会话存在 → 校验用户 active。返回用户 ID。
    pub async fn authenticate(&self, jwt_token: &str) -> Result<u64, IdentityError> {
        let claims = chargepilot_platform::jwt::verify(
            self.jwt_secret,
            chargepilot_platform::jwt::Kind::User,
            jwt_token,
        )
        .map_err(|_| IdentityError::TokenInvalid)?;
        if claims.sub == 0 {
            return Err(IdentityError::TokenInvalid);
        }
        if !session::exists(self.redis_url, &claims.sid)
            .await
            .map_err(IdentityError::from)?
        {
            return Err(IdentityError::SessionExpired);
        }
        if !repo::is_active(self.db, claims.sub)
            .await
            .map_err(IdentityError::from)?
        {
            return Err(IdentityError::AccountFrozen);
        }
        Ok(claims.sub)
    }

    /// profile 组合：用户字段 + 钱包只读投影。
    pub async fn profile(&self, user_id: u64) -> Result<Profile, IdentityError> {
        let p: UserProfile = repo::find_profile(self.db, user_id)
            .await
            .map_err(IdentityError::from)?
            .ok_or(IdentityError::TokenInvalid)?;
        let wallet = chargepilot_mod_wallet::account::find_by_user(self.db, user_id)
            .await
            .map_err(IdentityError::from)?;
        Ok(Profile {
            user_id: p.id,
            nickname: p.nickname,
            avatar_url: p.avatar_url,
            phone_bound: p.phone.is_some(),
            registered_at: p
                .registered_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            wallet: wallet
                .map(|w| WalletView {
                    status: w.status,
                    available_cents: w.balance_cents,
                    frozen_cents: w.frozen_cents,
                })
                .unwrap_or(WalletView {
                    status: "active".into(),
                    available_cents: 0,
                    frozen_cents: 0,
                }),
            coupon_unused_count: 0,
        })
    }
}

/// JWT 的 sid 取刷新令牌的 sid 段（profile 实时会话校验依赖）。
fn refresh_token_part(token: &str) -> String {
    token
        .strip_prefix(USER_TOKEN_LAYOUT.token_prefix)
        .and_then(|rest| rest.split_once('.'))
        .map(|(sid, _)| sid.to_string())
        .unwrap_or_default()
}

fn sign_access(secret: &str, user: &User, sid: String) -> anyhow::Result<String> {
    chargepilot_platform::jwt::sign_with_sid(
        secret,
        chargepilot_platform::jwt::Kind::User,
        user.id,
        sid,
    )
}
