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

// ---------- 事务内账户操作（旧版 wallet_account_repo.go 对齐） ----------

use crate::WalletError;

/// 钱包账户完整行（旧版 WalletRow）：充值与退款都要读余额与冻结额。
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: u64,
    pub user_id: u64,
    pub balance_cents: i64,
    pub frozen_cents: i64,
    pub status: crate::AccountStatus,
    pub version: u64,
}

/// 在调用方事务中锁 userID 未删除的钱包，返回当前行；缺账户为 NotFound。
/// 调用方负责提交，不自行开户（旧版 LockWalletAccount）。
pub async fn lock_wallet_account_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<AccountRow, WalletError> {
    let row: Option<(u64, u64, i64, i64, crate::AccountStatus, u64)> = sqlx::query_as(
        "SELECT `id`, `user_id`, `balance_cents`, `frozen_cents`, `status`, `version` \
         FROM `wallet_account` WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let (id, uid, balance, frozen, status, version) = row.ok_or(WalletError::NotFound)?;
    Ok(AccountRow {
        id,
        user_id: uid,
        balance_cents: balance,
        frozen_cents: frozen,
        status,
        version,
    })
}

/// 在调用方事务中为 userID 开户，初始余额、冻结额为零且状态 active（旧版 OpenWalletAccount）。
pub async fn open_wallet_account_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_account` (`user_id`, `balance_cents`, `frozen_cents`, `status`) \
         VALUES (?, 0, 0, ?)",
    )
    .bind(user_id as i64)
    .bind(crate::AccountStatus::Active)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 在调用方事务中读取 userID 未删除身份的 openid；缺用户或空身份返回空串
/// 由退款/充值用例拒绝（旧版 FindUserOpenID）。
pub async fn find_user_openid_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<String, WalletError> {
    // openid 列为 varbinary（微信加密标识原样字节），读取侧 CAST 为 CHAR。
    let openid: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CAST(`openid` AS CHAR) FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(openid.flatten().unwrap_or_default())
}

/// 在调用方事务中锁 userID 未删除的用户行（旧版 LockUser）；缺行 NotFound，不检查 active。
pub async fn lock_user_tx(tx: &mut MySqlConnection, user_id: u64) -> Result<(), WalletError> {
    let n = sqlx::query(
        "SELECT `id` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    match n {
        Some(_) => Ok(()),
        None => Err(WalletError::NotFound),
    }
}

/// 在调用方事务中将 walletID 钱包置为 frozen 并增加版本号；
/// 不改变余额或已冻结金额（旧版 FreezeWallet）。
pub async fn freeze_wallet_tx(tx: &mut MySqlConnection, wallet_id: u64) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE `wallet_account` SET `status` = ?, `version` = `version` + 1 WHERE `id` = ?",
    )
    .bind(crate::AccountStatus::Frozen)
    .bind(wallet_id as i64)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 将 walletID 钱包恢复 active 并增加版本号（旧版 ActivateWallet）。
pub async fn activate_wallet_tx(
    tx: &mut MySqlConnection,
    wallet_id: u64,
) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE `wallet_account` SET `status` = ?, `version` = `version` + 1 WHERE `id` = ?",
    )
    .bind(crate::AccountStatus::Active)
    .bind(wallet_id as i64)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
