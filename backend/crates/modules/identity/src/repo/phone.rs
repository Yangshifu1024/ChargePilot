//! user 表手机号绑定事务原语（蓝图第五章 repo 层）。

use crate::error::PhoneError;
use sqlx::{Acquire, MySqlPool};

/// 事务内查重并写入手机号。`value` 为 None 时解绑；
/// 号码已被他人占用返回 [`PhoneError::Taken`]。
pub async fn bind_phone(
    pool: &MySqlPool,
    user_id: u64,
    value: Option<&str>,
) -> Result<(), PhoneError> {
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
