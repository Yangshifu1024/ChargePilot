//! RFC 6238 TOTP：SHA1、6 位、30 秒步长，校验时允许当前步 ±1 的时钟偏移。

use base32::Alphabet;
use hmac::{Hmac, Mac};
use sha1::Sha1;

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = match Hmac::<Sha1>::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => panic!("TOTP 密钥不能为空"),
    };
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    bin % 1_000_000
}

/// 校验 6 位验证码；允许当前步 ±1 的时钟偏移。secret 为 base32（无填充）。
pub fn verify(secret_base32: &str, code: &str, at_unix: i64) -> bool {
    let Some(secret) = base32::decode(
        Alphabet::Rfc4648Lower { padding: false },
        &secret_base32.to_lowercase(),
    ) else {
        return false;
    };
    let Ok(parsed) = code.parse::<u32>() else {
        return false;
    };
    let step = at_unix.div_euclid(30);
    (0..=2).any(|offset| hotp(&secret, (step + offset - 1) as u64) == parsed)
}

/// 生成当前验证码（测试与展示用）。
pub fn generate(secret_base32: &str, at_unix: i64) -> Option<String> {
    let secret = base32::decode(
        Alphabet::Rfc4648Lower { padding: false },
        &secret_base32.to_lowercase(),
    )?;
    Some(format!(
        "{:06}",
        hotp(&secret, at_unix.div_euclid(30) as u64)
    ))
}

/// 生成随机 base32 密钥（20 字节 → 32 字符，RFC 4648 小写无填充）。
pub fn random_secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 20];
    rand::rng().fill_bytes(&mut b);
    base32::encode(Alphabet::Rfc4648Lower { padding: false }, &b)
}

/// 生成 otpauth:// URI（供绑定二维码扫描，算法/位数/周期固定）。
pub fn otpauth_uri(issuer: &str, account: &str, secret: &str) -> String {
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits=6&period=30"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 附录 B 参考向量（SHA1，secret=ASCII "12345678901234567890"）
    fn rfc_secret() -> String {
        base32::encode(
            Alphabet::Rfc4648Lower { padding: false },
            b"12345678901234567890",
        )
    }

    #[test]
    fn rfc6238_reference_vectors() {
        let s = rfc_secret();
        assert_eq!(generate(&s, 59).unwrap(), "287082");
        assert_eq!(generate(&s, 1111111109).unwrap(), "081804");
        assert_eq!(generate(&s, 1234567890).unwrap(), "005924");
    }

    #[test]
    fn verify_accepts_one_step_drift() {
        let s = rfc_secret();
        let t = 1111111109;
        assert!(verify(&s, "081804", t));
        // ±1 步：相邻步长的码都应通过
        let prev = generate(&s, t - 30).unwrap();
        let next = generate(&s, t + 30).unwrap();
        assert!(verify(&s, &prev, t));
        assert!(verify(&s, &next, t));
        // 超出 ±1 步拒绝
        let far = generate(&s, t - 90).unwrap();
        assert!(!verify(&s, &far, t));
    }
}
