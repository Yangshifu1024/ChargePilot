//! JWT 访问令牌：HS256 签名，分 user / admin 两类，TTL 分别为 15 分钟与 8 小时。

use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

/// 令牌受众类别（决定 TTL 与校验时的 kind 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Admin,
}

/// JWT 载荷（`sub` 为主体 ID，`sid` 绑定会话）。
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// kind：user / admin
    pub kind: String,
    /// 用户或管理员 ID
    pub sub: u64,
    /// 关联会话 ID（user 令牌用于实时会话校验；admin 可为空串）
    #[serde(default)]
    pub sid: String,
    /// 过期时间（Unix 秒）
    pub exp: u64,
    /// 签发时间
    pub iat: u64,
}

/// 用户访问令牌有效期（秒）。
pub const USER_TTL_SECS: u64 = 15 * 60;
/// 管理员访问令牌有效期（秒）。
pub const ADMIN_TTL_SECS: u64 = 8 * 60 * 60;

/// 签发访问令牌。
pub fn sign(secret: &str, kind: Kind, sub: u64) -> anyhow::Result<String> {
    sign_with_sid(secret, kind, sub, String::new())
}

/// 签发携带会话 ID 的访问令牌（user 令牌的实时会话校验依赖 sid）。
pub fn sign_with_sid(secret: &str, kind: Kind, sub: u64, sid: String) -> anyhow::Result<String> {
    let now = chrono::Utc::now().timestamp() as u64;
    let claims = Claims {
        kind: kind_str(kind).into(),
        sub,
        sid,
        exp: now + ttl(kind),
        iat: now,
    };
    Ok(encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

/// 校验并返回声明；kind 不符视为无效。
pub fn verify(secret: &str, kind: Kind, token: &str) -> anyhow::Result<Claims> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| anyhow::anyhow!("令牌无效: {e}"))?;
    if data.claims.kind != kind_str(kind) {
        anyhow::bail!("令牌种类不符");
    }
    Ok(data.claims)
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::User => "user",
        Kind::Admin => "admin",
    }
}

fn ttl(k: Kind) -> u64 {
    match k {
        Kind::User => USER_TTL_SECS,
        Kind::Admin => ADMIN_TTL_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_kind_isolation() {
        let secret = "0123456789abcdef0123456789abcdef";
        let t = sign(secret, Kind::User, 42).unwrap();
        let claims = verify(secret, Kind::User, &t).unwrap();
        assert_eq!(claims.sub, 42);
        // admin kind 不能用 user 令牌
        assert!(verify(secret, Kind::Admin, &t).is_err());
        // 错误密钥拒绝
        assert!(verify("ffffffffffffffffffffffffffffffff", Kind::User, &t).is_err());
    }
}
