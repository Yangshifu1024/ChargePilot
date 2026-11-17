//! C 端会话存储：Redis 记录 `{uid, openid, digest, previous[]}`，7 天绝对寿命，
//! 轮换/注销走 platform::sessiontoken 的 Lua 脚本。

use crate::IdentityError;
use chargepilot_platform::sessiontoken::{Layout, REVOKE_SCRIPT, ROTATE_SCRIPT, digest};

/// 会话 Redis 存活期：7 天（绝对寿命，轮换 KEEPTTL 不延长）。
pub const REFRESH_TTL_SECS: u64 = 7 * 24 * 3600;

pub const USER_TOKEN_LAYOUT: Layout = Layout {
    token_prefix: "RT_",
    key_prefix: "user:session:",
};

/// 轮换成功返回 (uid, openid, 新刷新令牌)。
pub struct Rotated {
    pub uid: u64,
    pub openid: String,
    pub refresh_token: String,
}

fn client(url: &str) -> anyhow::Result<redis::Client> {
    Ok(redis::Client::open(url)?)
}

/// 建会话：写记录并返回刷新令牌明文。
pub async fn create(redis_url: &str, user_id: u64, openid: &str) -> anyhow::Result<String> {
    let mut conn = client(redis_url)?
        .get_multiplexed_tokio_connection()
        .await?;
    let (sid, token, secret) = USER_TOKEN_LAYOUT.mint()?;
    let record = serde_json::json!({
        "uid": user_id.to_string(),   // 精确十进制字符串，防 cjson 浮点
        "openid": openid,
        "digest": digest(&secret),
        "previous": [],
    });
    redis::cmd("SET")
        .arg(USER_TOKEN_LAYOUT.key(&sid))
        .arg(record.to_string())
        .arg("EX")
        .arg(REFRESH_TTL_SECS)
        .query_async::<()>(&mut conn)
        .await?;
    Ok(token)
}

/// 原子轮换：校验旧摘要 → Lua 更新（KEEPTTL，保留 32 旧摘要）→ 返回用户身份与新令牌。
/// 轮换语义为「同键换密钥」：sid 不变、只换 secret。
pub async fn rotate(redis_url: &str, token: &str) -> Result<Rotated, IdentityError> {
    let Some((sid, secret)) = USER_TOKEN_LAYOUT.parse(token) else {
        return Err(IdentityError::InvalidRefresh);
    };
    let mut conn = client(redis_url)
        .map_err(IdentityError::from)?
        .get_multiplexed_tokio_connection()
        .await
        .map_err(anyhow::Error::from)
        .map_err(IdentityError::from)?;
    let (_gen_sid, _gen_token, new_secret) =
        USER_TOKEN_LAYOUT.mint().map_err(IdentityError::from)?;
    let rotated: Option<String> = redis::Script::new(ROTATE_SCRIPT)
        .key(USER_TOKEN_LAYOUT.key(&sid))
        .arg(digest(&secret))
        .arg(digest(&new_secret))
        .invoke_async(&mut conn)
        .await
        .map_err(anyhow::Error::from)
        .map_err(IdentityError::from)?;
    let Some(old_record) = rotated else {
        return Err(IdentityError::InvalidRefresh);
    };
    let record: serde_json::Value = serde_json::from_str(&old_record)
        .map_err(|e| IdentityError::Storage(anyhow::anyhow!("会话记录损坏: {e}")))?;
    let uid: u64 = record["uid"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(IdentityError::InvalidRefresh)?;
    let openid = record["openid"].as_str().unwrap_or_default().to_string();
    let refresh_token = format!("{}{}.{}", USER_TOKEN_LAYOUT.token_prefix, sid, new_secret);
    Ok(Rotated {
        uid,
        openid,
        refresh_token,
    })
}

/// 撤销会话：当前或历史摘要命中即删。返回是否撤销成功。
pub async fn revoke(redis_url: &str, token: &str) -> anyhow::Result<bool> {
    let Some((sid, secret)) = USER_TOKEN_LAYOUT.parse(token) else {
        return Ok(false);
    };
    let mut conn = client(redis_url)?
        .get_multiplexed_tokio_connection()
        .await?;
    let revoked: i32 = redis::Script::new(REVOKE_SCRIPT)
        .key(USER_TOKEN_LAYOUT.key(&sid))
        .arg(digest(&secret))
        .invoke_async(&mut conn)
        .await?;
    Ok(revoked == 1)
}

/// 会话是否仍存在（profile 的实时校验：JWT 有效但会话已注销 → SessionExpired）。
pub async fn exists(redis_url: &str, sid: &str) -> anyhow::Result<bool> {
    let mut conn = client(redis_url)?
        .get_multiplexed_tokio_connection()
        .await?;
    let found: Option<String> = redis::cmd("GET")
        .arg(USER_TOKEN_LAYOUT.key(sid))
        .query_async(&mut conn)
        .await?;
    Ok(found.is_some())
}
