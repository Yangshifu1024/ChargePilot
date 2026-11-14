//! 手机号绑定与解绑（旧版 internal/identity/phone.go + user_repo.go BindPhone
//! 契约对齐）。号码来源按登录模式分流，语义与旧版 PhoneService 一致：
//! - development 模式：接受明文 `phone` 输入（旧版 DevelopmentPhoneSource），
//!   照常走归一化与格式校验，空号或非法号按输入错误拒绝；
//! - wechat 模式：只接受微信手机号授权 `code` 且不得同时提交明文
//!   （旧版 WeChatPhoneSource.Resolve 的准入判定）；正式号码交换能力接入前
//!   统一按「暂不可用」拒绝，不采信客户端明文。
//!
//! 归属写入与旧版 UserRepo.BindPhone 一致：事务内先按 `phone + id <> + 未删除`
//! 查重，再更新本人未删除行的 `phone`；并发同号由 `uk_phone` 唯一键最终仲裁。

use crate::wechat::Exchanger;
use sqlx::Acquire;
use sqlx::MySqlPool;

/// 手机号业务错误（HTTP 映射沿用旧版字面量契约，见 http-api 适配层）。
#[derive(Debug, thiserror::Error)]
pub enum PhoneError {
    /// 正式路径须凭授权 code 且不得提交明文（400/1005）。
    #[error("phone authorization input invalid")]
    Authorization,
    /// 正式号码交换能力未装配（503/5003）。
    #[error("phone exchange unavailable")]
    Unavailable,
    /// 微信号码交换失败，不能采信客户端号码（400/1004）。
    #[error("phone exchange failed")]
    Exchange,
    /// 号码不符合中国大陆手机号约束（400/1005）。
    #[error("phone input invalid")]
    Input,
    /// 号码已被他人绑定（409/2009）。
    #[error("phone already taken")]
    Taken,
    /// 存储错误（503/5003）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for PhoneError {
    fn from(e: sqlx::Error) -> Self {
        // 并发同号最终由 uk_phone 唯一键仲裁（旧版 1062 → ErrPhoneTaken）。
        if let sqlx::Error::Database(db) = &e
            && db.code().as_deref() == Some("23000")
        {
            return PhoneError::Taken;
        }
        PhoneError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 绑定请求字段（旧版 PhoneInput）：正式路径仅收 `code`，开发路径用 `phone`。
#[derive(Debug, Clone, Default)]
pub struct PhoneInput {
    /// 开发输入的号码；正式绑定不得填写。
    pub phone: Option<String>,
    /// 微信手机号授权凭证。
    pub code: Option<String>,
}

/// 绑定成功响应（旧版 PhoneResult）：只返回掩码，不回显原号码。
#[derive(Debug, serde::Serialize)]
pub struct PhoneResult {
    pub bound: bool,
    pub phone_masked: String,
}

/// 去除号码首尾空白（旧版 phone.Normalize）。
pub fn normalize(value: &str) -> String {
    value.trim().to_string()
}

/// 校验 11 位中国大陆手机号：1 开头、第二位 3-9、全数字（旧版 phone.Valid）。
pub fn valid(value: &str) -> bool {
    let v = value.trim().as_bytes();
    v.len() == 11
        && v[0] == b'1'
        && (b'3'..=b'9').contains(&v[1])
        && v.iter().all(|c| c.is_ascii_digit())
}

/// 掩码展示：保留前 3 后 4，中间 4 星；不足 7 字节返回 ***（旧版 phone.Mask）。
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

/// 解析号码来源并校验，成功后绑定到当前用户（旧版 PhoneService.Bind）。
/// 失败不声称绑定成功，也不记录原号码或凭据。
pub async fn bind(
    pool: &MySqlPool,
    exchanger: &Exchanger,
    user_id: u64,
    input: &PhoneInput,
) -> Result<PhoneResult, PhoneError> {
    let value = match exchanger {
        // 开发模式：明文输入不经过正式授权实现（旧版 DevelopmentPhoneSource）。
        Exchanger::Development => input.phone.clone().unwrap_or_default(),
        Exchanger::Wechat { .. } => {
            // 正式路径准入：必须提交 code 且不得携带明文（旧版 WeChatPhoneSource.Resolve）。
            let code = input.code.as_deref().unwrap_or("");
            let has_phone = input.phone.as_deref().is_some_and(|p| !p.is_empty());
            if code.is_empty() || has_phone {
                return Err(PhoneError::Authorization);
            }
            // 微信手机号交换能力未接入：显式按暂不可用拒绝，不采信客户端明文
            // （旧版 Exchange == nil → ErrPhoneUnavailable 的诚实语义）。
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

/// 解除当前用户手机号绑定：号码原子设为 NULL（旧版 PhoneService.Unbind）。
/// 未绑定用户重复解绑同样成功（UPDATE 零行不视为错误，旧版语义）。
pub async fn unbind(pool: &MySqlPool, user_id: u64) -> Result<(), PhoneError> {
    bind_phone(pool, user_id, None).await
}

/// 事务内查重并写入手机号（旧版 UserRepo.BindPhone）。
/// `value` 为 None 时解绑；号码已被他人占用返回 `PhoneError::Taken`。
async fn bind_phone(pool: &MySqlPool, user_id: u64, value: Option<&str>) -> Result<(), PhoneError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    if let Some(v) = value {
        let holder: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM `user` \
             WHERE `phone` = ? AND `id` <> ? AND `deleted_at` IS NULL",
        )
        .bind(v)
        .bind(user_id as i64)
        .fetch_one(&mut *tx)
        .await?;
        if holder > 0 {
            return Err(PhoneError::Taken);
        }
    }
    sqlx::query("UPDATE `user` SET `phone` = ? WHERE `id` = ? AND `deleted_at` IS NULL")
        .bind(value)
        .bind(user_id as i64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
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
        // 旧版 Valid 内部先 Normalize（trim）再匹配，带空白号码同样通过。
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
