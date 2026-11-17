//! 口令哈希：bcrypt，须与库内既有 `$2a$` 哈希行互通校验。

/// 哈希口令（cost=10，与既有库中 bcrypt 行一致）。
pub fn hash(plain: &str) -> anyhow::Result<String> {
    Ok(bcrypt::hash(plain, 10)?)
}

/// 校验口令；兼容 `$2a$/$2b$/$2y$` 前缀（bcrypt crate 自动处理）。
pub fn verify(plain: &str, hashed: &str) -> bool {
    bcrypt::verify(plain, hashed).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let h = super::hash("s3cret!").unwrap();
        assert!(h.starts_with("$2"));
        assert!(super::verify("s3cret!", &h));
        assert!(!super::verify("wrong", &h));
    }

    #[test]
    fn accepts_two_a_prefix_hashes() {
        // $2a$ 与 $2b$ 算法一致：库内常见 $2a$ 前缀哈希须能通过 verify
        let as_b = super::hash("legacy-pass").unwrap();
        let as_a = as_b.replacen("$2b$", "$2a$", 1);
        assert!(as_a.starts_with("$2a$"));
        assert!(super::verify("legacy-pass", &as_a));
    }
}
