//! 钱包账户：登录建户（零余额）与只读查询。

use sqlx::MySqlConnection;

/// 钱包账户只读投影（profile 展示用）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccountView {
    pub status: String,
    pub balance_cents: i64,
    pub frozen_cents: i64,
}

/// 在业务事务内为新建用户创建零余额钱包（与用户建户同事务，旧版语义）。
/// `account_id` 由调用方分配（与用户 ID 同源号段，旧版两者取同一 ID）。
pub async fn ensure_account_tx(
    tx: &mut MySqlConnection,
    account_id: u64,
    user_id: u64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO `wallet_account` (`id`, `user_id`) VALUES (?, ?)")
        .bind(account_id as i64)
        .bind(user_id as i64)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 按用户读取钱包账户；无钱包返回 None（理论上登录即建户，None 属异常数据）。
pub async fn find_by_user(
    pool: &sqlx::MySqlPool,
    user_id: u64,
) -> anyhow::Result<Option<AccountView>> {
    let row = sqlx::query_as::<_, AccountView>(
        "SELECT `status`, `balance_cents`, `frozen_cents` FROM `wallet_account` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
