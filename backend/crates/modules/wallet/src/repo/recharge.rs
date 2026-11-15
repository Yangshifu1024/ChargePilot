//! 充值申请表访问：wallet_recharge_request（蓝图第五章 repo 层）。

use crate::WalletError;
use sqlx::MySqlConnection;

/// 锁读既有申请（user_id, amount_cents, payment_order_id）；无记录返回 None。
pub async fn lock_request_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, i64, u64)>, WalletError> {
    sqlx::query_as(
        "SELECT `user_id`, `amount_cents`, `payment_order_id` FROM `wallet_recharge_request` \
         WHERE `request_id` = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 保存申请行（request_json 为幂等存档）。
pub async fn insert_request_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    user_id: u64,
    amount_cents: i64,
    payment_order_id: u64,
    request_json: &str,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_recharge_request` (`request_id`, `user_id`, \
         `amount_cents`, `payment_order_id`, `request_json`) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(user_id as i64)
    .bind(amount_cents)
    .bind(payment_order_id as i64)
    .bind(request_json)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
