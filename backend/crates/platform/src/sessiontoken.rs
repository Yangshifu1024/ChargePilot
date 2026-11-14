//! 刷新令牌：`<前缀><sid>.<secret>` 编码 + SHA-256 摘要 + Redis Lua 轮换/注销脚本。
//! 脚本自旧版逐字节移植（保留 32 个旧摘要、KEEPTTL、整数字段防 cjson 浮点）。

use base64::Engine as _;

/// 令牌与存储键的前缀组（调用方定义来源，如 `RT_`/`user:session:`）。
#[derive(Debug, Clone)]
pub struct Layout {
    pub token_prefix: &'static str,
    pub key_prefix: &'static str,
}

impl Layout {
    pub fn key(&self, sid: &str) -> String {
        format!("{}{}", self.key_prefix, sid)
    }

    /// 生成 (sid, 完整令牌, secret)；sid 16 字节、secret 32 字节，无填充 base64url。
    pub fn mint(&self) -> anyhow::Result<(String, String, String)> {
        let sid = random_b64(16)?;
        let secret = random_b64(32)?;
        Ok((
            sid.clone(),
            format!("{}{}.{}", self.token_prefix, sid, secret),
            secret,
        ))
    }

    /// 解析令牌：前缀、两段长度（22/43）与 base64url 合法性。格式错误无需查 Redis。
    pub fn parse(&self, token: &str) -> Option<(String, String)> {
        let rest = token.strip_prefix(self.token_prefix)?;
        let (sid, secret) = rest.split_once('.')?;
        if sid.len() != 22 || secret.len() != 43 {
            return None;
        }
        B64.decode(sid).ok()?;
        B64.decode(secret).ok()?;
        Some((sid.to_string(), secret.to_string()))
    }
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn random_b64(n: usize) -> anyhow::Result<String> {
    use rand::RngCore;
    let mut b = vec![0u8; n];
    rand::rng().fill_bytes(&mut b);
    Ok(B64.encode(b))
}

/// secret 的 SHA-256 十六进制摘要。
pub fn digest(secret: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(secret.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 轮换脚本：核对旧摘要 → 更新为新摘要 → 保留 32 个旧摘要 → KEEPTTL。
/// ARGV[3..] 为需保留精度的整数字段（防 cjson 把整数变浮点）；返回轮换前的记录，失败返回 false。
pub const ROTATE_SCRIPT: &str = r#"
local value = redis.call('GET', KEYS[1])
if not value then return false end
local record = cjson.decode(value)
if record.digest ~= ARGV[1] then return false end
for i = 3, #ARGV do
  local field = ARGV[i]
  if type(record[field]) == 'number' then
    local original = string.match(value, '"' .. field .. '"%s*:%s*(%d+)%s*[,}]')
    if not original then return false end
    record[field] = original
  end
end
record.digest = ARGV[2]
record.previous = record.previous or {}
table.insert(record.previous, ARGV[1])
if #record.previous > 32 then table.remove(record.previous, 1) end
redis.call('SET', KEYS[1], cjson.encode(record), 'KEEPTTL')
return value
"#;

/// 注销脚本：当前摘要或 32 个历史摘要命中即删除整个会话键；返回 1/0。
pub const REVOKE_SCRIPT: &str = r#"
local value = redis.call('GET', KEYS[1])
if not value then return 0 end
local record = cjson.decode(value)
if record.digest == ARGV[1] then redis.call('DEL', KEYS[1]); return 1 end
for _, digest in ipairs(record.previous or {}) do
  if digest == ARGV[1] then redis.call('DEL', KEYS[1]); return 1 end
end
return 0
"#;

/// 生成随机挑战码（base64url，无填充）——MFA 挑战等场景。
pub fn random_token(byte_len: usize) -> String {
    // 复用 mint 的随机源：直接生成
    fn inner(n: usize) -> anyhow::Result<String> {
        use rand::RngCore;
        let mut b = vec![0u8; n];
        rand::rng().fill_bytes(&mut b);
        Ok(B64.encode(b))
    }
    inner(byte_len).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_parse_round_trip() {
        let layout = Layout {
            token_prefix: "RT_",
            key_prefix: "user:session:",
        };
        let (sid, token, secret) = layout.mint().unwrap();
        let (sid2, secret2) = layout.parse(&token).unwrap();
        assert_eq!(sid, sid2);
        assert_eq!(secret, secret2);
        // 格式攻击：错前缀、错长度、非 base64
        assert!(layout.parse("XX_abc.def").is_none());
        assert!(layout.parse("RT_a.b").is_none());
        assert!(
            layout
                .parse("RT_!!!!!.???????????????????????????????????????")
                .is_none()
        );
    }

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(digest("abc").len(), 64);
        assert_eq!(digest("abc"), digest("abc"));
        assert_ne!(digest("abc"), digest("abd"));
    }

    #[ignore = "真库集成：node tasks.mjs test-integration"]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
    async fn rotate_and_revoke_scripts() {
        // 门控语义：未设 TEST_REDIS_URL 属调用方式错误，判失败而非静默跳过。
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            panic!("集成测试需要 TEST_REDIS_URL（node tasks.mjs test-integration）");
        };
        if url.is_empty() {
            panic!("集成测试需要 TEST_REDIS_URL（node tasks.mjs test-integration）");
        }
        let client = redis::Client::open(url).unwrap();
        let mut conn = client.get_multiplexed_tokio_connection().await.unwrap();

        let layout = Layout {
            token_prefix: "ART_",
            key_prefix: "test:session:",
        };
        let (sid, _token, secret) = layout.mint().unwrap();
        let key = layout.key(&sid);
        let d1 = digest(&secret);

        // 建会话
        redis::cmd("SET")
            .arg(&key)
            .arg(serde_json::json!({"digest": d1, "uid": 7, "previous": []}).to_string())
            .arg("EX")
            .arg(60)
            .query_async::<()>(&mut conn)
            .await
            .unwrap();

        // 轮换：旧摘要 → 新摘要，uid 字段保留整数精度
        let (sid2, _t2, secret2) = layout.mint().unwrap();
        let d2 = digest(&secret2);
        let rotated: String = redis::Script::new(ROTATE_SCRIPT)
            .key(&key)
            .arg(&d1)
            .arg(&d2)
            .arg("uid")
            .invoke_async(&mut conn)
            .await
            .unwrap();
        assert!(
            rotated.contains("\"uid\":7"),
            "整数字段必须原样保留: {rotated}"
        );

        // 旧摘要仍可注销（历史摘要命中）
        let revoked: i32 = redis::Script::new(REVOKE_SCRIPT)
            .key(&key)
            .arg(&d1)
            .invoke_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(revoked, 1);
        let gone: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(gone.is_none());
        let _ = sid2;
    }
}
