//! 后台会话（`ART_` / `admin:session:`，7 天绝对寿命 + Lua 轮换）、
//! MFA 挑战（`admin:mfa:<token>`，5 分钟）与登录频次（`admin:login-rate:<ip>`，60 秒窗口）。

use super::{AccountIdentity, OpsError};
use chargepilot_platform::sessiontoken::{Layout, REVOKE_SCRIPT, ROTATE_SCRIPT, digest};

pub const REFRESH_TTL_SECS: u64 = 7 * 24 * 3600;
const MFA_TTL_SECS: u64 = 5 * 60;
const RATE_WINDOW_SECS: u64 = 60;
/// 窗口内允许的登录尝试次数（第 6 次拒绝，旧版语义 n>5）。
const RATE_LIMIT: i64 = 5;

pub const ADMIN_TOKEN_LAYOUT: Layout = Layout {
    token_prefix: "ART_",
    key_prefix: "admin:session:",
};

async fn conn(redis_url: &str) -> anyhow::Result<redis::aio::MultiplexedConnection> {
    Ok(redis::Client::open(redis_url)?
        .get_multiplexed_tokio_connection()
        .await?)
}

/// 创建后台会话，返回刷新令牌明文。
pub async fn create(redis_url: &str, identity: &AccountIdentity) -> anyhow::Result<String> {
    let mut conn = conn(redis_url).await?;
    let (sid, token, secret) = ADMIN_TOKEN_LAYOUT.mint()?;
    let record = serde_json::json!({
        "version": identity.auth_version.to_string(), // 精确字符串，防 cjson 浮点
        "uid": identity.id.to_string(),
        "username": identity.username,
        "digest": digest(&secret),
        "previous": [],
    });
    redis::cmd("SET")
        .arg(ADMIN_TOKEN_LAYOUT.key(&sid))
        .arg(record.to_string())
        .arg("EX")
        .arg(REFRESH_TTL_SECS)
        .query_async::<()>(&mut conn)
        .await?;
    Ok(token)
}

/// 轮换（同键换密钥），返回账号身份与新令牌。
pub async fn rotate(redis_url: &str, token: &str) -> Result<(AccountIdentity, String), OpsError> {
    let Some((sid, secret)) = ADMIN_TOKEN_LAYOUT.parse(token) else {
        return Err(OpsError::Credentials);
    };
    let mut conn = conn(redis_url).await.map_err(OpsError::from)?;
    let (_g1, _g2, new_secret) = ADMIN_TOKEN_LAYOUT.mint().map_err(OpsError::from)?;
    let rotated: Option<String> = redis::Script::new(ROTATE_SCRIPT)
        .key(ADMIN_TOKEN_LAYOUT.key(&sid))
        .arg(digest(&secret))
        .arg(digest(&new_secret))
        .arg("version") // 整数字段保留精度
        .invoke_async(&mut conn)
        .await
        .map_err(OpsError::from)?;
    let Some(old) = rotated else {
        return Err(OpsError::Credentials);
    };
    let record: serde_json::Value = serde_json::from_str(&old)
        .map_err(|e| OpsError::Storage(anyhow::anyhow!("会话记录损坏: {e}")))?;
    let id = record["uid"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(OpsError::Credentials)?;
    let version = record["version"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(OpsError::Credentials)?;
    let username = record["username"].as_str().unwrap_or_default().to_string();
    Ok((
        AccountIdentity {
            id,
            auth_version: version,
            username,
        },
        format!("{}{}.{}", ADMIN_TOKEN_LAYOUT.token_prefix, sid, new_secret),
    ))
}

/// 撤销（当前或历史摘要命中）。
pub async fn revoke(redis_url: &str, token: &str) -> anyhow::Result<bool> {
    let Some((sid, secret)) = ADMIN_TOKEN_LAYOUT.parse(token) else {
        return Ok(false);
    };
    let mut conn = conn(redis_url).await?;
    let n: i32 = redis::Script::new(REVOKE_SCRIPT)
        .key(ADMIN_TOKEN_LAYOUT.key(&sid))
        .arg(digest(&secret))
        .invoke_async(&mut conn)
        .await?;
    Ok(n == 1)
}

/// 会话存在性（实时鉴权用）。
pub async fn exists(redis_url: &str, sid: &str) -> anyhow::Result<bool> {
    let mut conn = conn(redis_url).await?;
    let found: Option<String> = redis::cmd("GET")
        .arg(ADMIN_TOKEN_LAYOUT.key(sid))
        .query_async(&mut conn)
        .await?;
    Ok(found.is_some())
}

/// 登录频次：60 秒窗口 INCR，超过 5 次拒绝（旧版 n>5 拒绝）。
pub async fn check_rate(redis_url: &str, ip: &str) -> Result<(), OpsError> {
    let mut conn = conn(redis_url).await.map_err(OpsError::from)?;
    let key = format!("admin:login-rate:{ip}");
    let (n, _): (i64, i64) = redis::pipe()
        .atomic()
        .cmd("INCR")
        .arg(&key)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(RATE_WINDOW_SECS)
        .arg("NX")
        .query_async(&mut conn)
        .await
        .map_err(OpsError::from)?;
    if n > RATE_LIMIT {
        return Err(OpsError::LoginRate);
    }
    Ok(())
}

/// 为通过密码校验的账号写入 5 分钟 MFA 挑战，返回挑战码。
pub async fn begin_mfa(redis_url: &str, identity: &AccountIdentity) -> anyhow::Result<String> {
    let mut conn = conn(redis_url).await?;
    let token = chargepilot_platform::sessiontoken::random_token(24);
    let record = serde_json::json!({
        "id": identity.id.to_string(),
        "auth_version": identity.auth_version.to_string(),
        "username": identity.username,
    });
    redis::cmd("SET")
        .arg(format!("admin:mfa:{token}"))
        .arg(record.to_string())
        .arg("EX")
        .arg(MFA_TTL_SECS)
        .query_async::<()>(&mut conn)
        .await?;
    Ok(token)
}

/// 读取挑战（不删除；坏码可重试）。
pub async fn resolve_mfa(redis_url: &str, token: &str) -> Result<AccountIdentity, OpsError> {
    let mut conn = conn(redis_url).await.map_err(OpsError::from)?;
    let raw: Option<String> = redis::cmd("GET")
        .arg(format!("admin:mfa:{token}"))
        .query_async(&mut conn)
        .await
        .map_err(OpsError::from)?;
    let Some(raw) = raw else {
        return Err(OpsError::InvalidChallenge);
    };
    let record: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| OpsError::Storage(anyhow::anyhow!("挑战记录损坏: {e}")))?;
    Ok(AccountIdentity {
        id: record["id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or(OpsError::InvalidChallenge)?,
        auth_version: record["auth_version"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        username: record["username"].as_str().unwrap_or_default().to_string(),
    })
}

/// 原子消费挑战（比较并删除，拒绝已替换记录；返回是否成功）。
pub async fn consume_mfa(
    redis_url: &str,
    token: &str,
    expected: &AccountIdentity,
) -> anyhow::Result<bool> {
    const CONSUME: &str = r#"
local value = redis.call('GET', KEYS[1])
if not value then return 0 end
local record = cjson.decode(value)
if record.id ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
return 1
"#;
    let mut conn = conn(redis_url).await?;
    let n: i32 = redis::Script::new(CONSUME)
        .key(format!("admin:mfa:{token}"))
        .arg(expected.id.to_string())
        .invoke_async(&mut conn)
        .await?;
    Ok(n == 1)
}

/// 读会话记录（实时鉴权用）：返回账号身份（含会话内认证版本）。
pub async fn find(redis_url: &str, sid: &str) -> anyhow::Result<Option<AccountIdentity>> {
    let mut conn = conn(redis_url).await?;
    let raw: Option<String> = redis::cmd("GET")
        .arg(ADMIN_TOKEN_LAYOUT.key(sid))
        .query_async(&mut conn)
        .await?;
    let Some(raw) = raw else { return Ok(None) };
    let record: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(Some(AccountIdentity {
        id: record["uid"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        auth_version: record["version"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        username: record["username"].as_str().unwrap_or_default().to_string(),
    }))
}
