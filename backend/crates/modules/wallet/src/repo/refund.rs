//! 退款申请与拆单预占表访问：wallet_refund_request / wallet_refund_part /
//! risk_freeze_log / wallet_risk_freeze_link 与 wallet_account 预占（蓝图第五章 repo 层）。

use crate::WalletError;
use sqlx::MySqlConnection;

/// 锁读既有申请用于幂等重放（user_id, reason, response_json）。
/// response_json 为 json 列、reason 可能为 ascii_bin：读取侧 CAST。
pub async fn lock_request_replay_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, Option<String>, String)>, WalletError> {
    sqlx::query_as(
        "SELECT `user_id`, CAST(`reason` AS CHAR), CAST(`response_json` AS CHAR) \
         FROM `wallet_refund_request` WHERE `request_id` = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 保存申请行（reason 空时存 NULL）。
pub async fn insert_request_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    user_id: u64,
    amount_cents: i64,
    reason: &Option<String>,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_refund_request` (`request_id`, `user_id`, `amount_cents`, `reason`) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(user_id as i64)
    .bind(amount_cents)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 5 分钟窗口内申请数（频次风控，含本次）。
pub async fn request_count_since_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<i64, WalletError> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM `wallet_refund_request` WHERE `user_id` = ? AND `created_at` >= ?",
    )
    .bind(user_id as i64)
    .bind(since)
    .fetch_one(&mut *tx)
    .await?;
    Ok(count)
}

/// 建频次冻结记录，返回主键。
pub async fn insert_frequency_freeze_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<i64, WalletError> {
    let freeze_id: i64 = sqlx::query(
        "INSERT INTO `risk_freeze_log` (`user_id`, `trigger_rule`, `frozen_action`, `reason`) \
         VALUES (?, 'wallet_refund_frequency', 'wallet', 'wallet refund frequency')",
    )
    .bind(user_id as i64)
    .execute(&mut *tx)
    .await?
    .last_insert_id() as i64;
    Ok(freeze_id)
}

/// 申请与冻结记录关联。
pub async fn link_freeze_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    freeze_id: i64,
) -> Result<(), WalletError> {
    sqlx::query("INSERT INTO `wallet_risk_freeze_link` (`request_id`, `freeze_id`) VALUES (?, ?)")
        .bind(request_id)
        .bind(freeze_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 保存回执（response_json 为 json 列：绑定 Value，String 绑定会 3140）。
pub async fn save_response_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    response_json: &serde_json::Value,
) -> Result<(), WalletError> {
    sqlx::query("UPDATE `wallet_refund_request` SET `response_json` = ? WHERE `request_id` = ?")
        .bind(response_json)
        .bind(request_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 事务内锁读钱包余额（id, balance_cents, frozen_cents）。
pub async fn lock_wallet_balance_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<Option<(u64, i64, i64)>, WalletError> {
    sqlx::query_as(
        "SELECT `id`, `balance_cents`, `frozen_cents` FROM `wallet_account` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 既有拆单数（防重复预占）。
pub async fn part_count_tx(tx: &mut MySqlConnection, request_id: &str) -> Result<i64, WalletError> {
    let (existing,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM `wallet_refund_part` WHERE `request_id` = ?")
            .bind(request_id)
            .fetch_one(&mut *tx)
            .await?;
    Ok(existing)
}

/// 写拆单行。
pub async fn insert_part_tx(
    tx: &mut MySqlConnection,
    refund_record_id: u64,
    request_id: &str,
    wallet_account_id: u64,
    amount_cents: i64,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_refund_part` (`refund_record_id`, `request_id`, \
         `wallet_account_id`, `amount_cents`, `settled`) VALUES (?, ?, ?, ?, FALSE)",
    )
    .bind(refund_record_id as i64)
    .bind(request_id)
    .bind(wallet_account_id)
    .bind(amount_cents)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 预占余额（冻结额累加，版本递增）。
pub async fn freeze_balance_tx(
    tx: &mut MySqlConnection,
    wallet_account_id: u64,
    amount_cents: i64,
) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE `wallet_account` SET `frozen_cents` = `frozen_cents` + ?, \
         `version` = `version` + 1 WHERE `id` = ?",
    )
    .bind(amount_cents)
    .bind(wallet_account_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
