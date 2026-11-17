//! 手机号绑定与解绑。号码来源按登录模式分流：
//! - development 模式：接受明文 `phone`，归一化与格式校验，非法号拒绝；
//! - wechat 模式：只接受微信授权 `code` 且不得同时提交明文；交换能力未接入前统一按「暂不可用」拒绝。
//!
//! 归属写入：事务内按「手机号 + 非本人 + 未删除」查重，再更新本人行的 `phone`；
//! 并发同号由 `uk_phone` 唯一键仲裁。

use crate::error::PhoneError;
use crate::repo::phone::bind_phone;
use crate::service::wechat::Exchanger;
use sqlx::MySqlPool;

/// 绑定请求字段：正式路径仅收 `code`，开发路径用 `phone`。
#[derive(Debug, Clone, Default)]
pub struct PhoneInput {
    /// 开发输入的号码；正式绑定不得填写。
    pub phone: Option<String>,
    /// 微信手机号授权凭证。
    pub code: Option<String>,
}

/// 绑定成功响应：只返回掩码，不回显原号码。
#[derive(Debug, serde::Serialize)]
pub struct PhoneResult {
    pub bound: bool,
    pub phone_masked: String,
}

/// 去除号码首尾空白。
pub fn normalize(value: &str) -> String {
    value.trim().to_string()
}

/// 校验 11 位中国大陆手机号：1 开头、第二位 3-9、全数字。
pub fn valid(value: &str) -> bool {
    let v = value.trim().as_bytes();
    v.len() == 11
        && v[0] == b'1'
        && (b'3'..=b'9').contains(&v[1])
        && v.iter().all(|c| c.is_ascii_digit())
}

/// 掩码展示：保留前 3 后 4，中间 4 星；不足 7 字节返回 ***。
pub fn mask(value: &str) -> String {
    let v = value.trim().as_bytes();
    if v.len() < 7 {
        return "***".into();
    }
    format!(
        "{}****{}",
        String::from_utf8_lossy(&v[..3]),
        String::from_utf8_lossy(&v[v.len() - 4..])
    )
}

/// 解析号码来源并校验，成功后绑定到当前用户。
/// 失败不声称绑定成功，也不记录原号码或凭据。
pub async fn bind(
    pool: &MySqlPool,
    exchanger: &Exchanger,
    user_id: u64,
    input: &PhoneInput,
) -> Result<PhoneResult, PhoneError> {
    let value = match exchanger {
        // 开发模式：明文输入不经过正式授权实现。
        Exchanger::Development => input.phone.clone().unwrap_or_default(),
        Exchanger::Wechat { .. } => {
            // 正式路径准入：必须提交 code 且不得携带明文。
            let code = input.code.as_deref().unwrap_or("");
            let has_phone = input.phone.as_deref().is_some_and(|p| !p.is_empty());
            if code.is_empty() || has_phone {
                return Err(PhoneError::Authorization);
            }
            // 微信手机号交换能力未接入：显式按暂不可用拒绝，不采信客户端明文
            //。
            return Err(PhoneError::Unavailable);
        }
    };
    let value = normalize(&value);
    if !valid(&value) {
        return Err(PhoneError::Input);
    }
    bind_phone(pool, user_id, Some(&value)).await?;
    Ok(PhoneResult {
        bound: true,
        phone_masked: mask(&value),
    })
}

/// 解除当前用户手机号绑定：号码原子设为 NULL。
/// 未绑定用户重复解绑同样成功。
pub async fn unbind(pool: &MySqlPool, user_id: u64) -> Result<(), PhoneError> {
    bind_phone(pool, user_id, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_valid_rejects_non_mainland() {
        assert!(valid("13812345678"));
        assert!(!valid("12812345678")); // 第二位 < 3
        assert!(!valid("198123456789")); // 超长
        assert!(!valid("1381234567")); // 短缺
        assert!(!valid("1381234567a")); // 非数字
        assert!(!valid(""));
        // ）再匹配，带空白号码同样通过。
        assert!(valid(" 13812345678"));
    }

    #[test]
    fn phone_mask_keeps_prefix_suffix() {
        assert_eq!(mask("13812345678"), "138****5678");
        assert_eq!(mask(" 13812345678 "), "138****5678");
        assert_eq!(mask("123"), "***");
        assert_eq!(mask(""), "***");
    }

    #[tokio::test]
    async fn wechat_mode_requires_code_without_plain_phone() {
        // 微信模式：明文与 code 互斥；能力未接入统一按暂不可用拒绝。
        let e = Exchanger::Wechat {
            appid: "a".into(),
            secret: "s".into(),
        };
        let pool = MySqlPool::connect_lazy("mysql://invalid:invalid@127.0.0.1:1/none").unwrap();
        // 仅明文 → 授权输入错误（先于任何数据库访问）。
        let err = bind(
            &pool,
            &e,
            1,
            &PhoneInput {
                phone: Some("13812345678".into()),
                code: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PhoneError::Authorization));
        // 仅 code（能力未接入）→ 暂不可用。
        let err = bind(
            &pool,
            &e,
            1,
            &PhoneInput {
                phone: None,
                code: Some("auth-code".into()),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PhoneError::Unavailable));
        // 两者都带 → 授权输入错误。
        let err = bind(
            &pool,
            &e,
            1,
            &PhoneInput {
                phone: Some("13812345678".into()),
                code: Some("auth-code".into()),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PhoneError::Authorization));
    }
}
