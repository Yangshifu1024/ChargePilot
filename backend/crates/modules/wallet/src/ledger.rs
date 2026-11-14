//! 钱包账本基础操作：行锁读取与余额变动流水（旧版 wallet_ledger_repo.go 逐行对齐）。
//! 只触及 wallet_account / wallet_txn 两表，供刷卡、结算、启动失败退款等
//! 跨模块事务复用；错误语义以本模块 WalletError 表达，调用方按业务映射。

use super::{AccountStatus, TxnBizType, TxnDirection, WalletError};
use sqlx::MySqlConnection;

/// wallet_account 的行锁查询结果（旧版 Row）。
#[derive(Debug, Clone)]
pub struct WalletRow {
    /// 已锁钱包主键。
    pub id: u64,
    /// 当前账面余额，分。
    pub balance_cents: i64,
    /// 当前冻结金额，分，不能出账使用。
    pub frozen_cents: i64,
    /// active 可消费，其他状态拒绝出账。
    pub status: AccountStatus,
}

/// 在调用方事务中以 FOR UPDATE 锁读取 user 的未删除账户（旧版 Lock）。
/// 缺账户返回 NotFound；调用方须持有钱包锁后再锁支付，并负责提交或回滚。
pub async fn lock_tx(tx: &mut MySqlConnection, user_id: u64) -> Result<WalletRow, WalletError> {
    let row: Option<(u64, i64, i64, String)> = sqlx::query_as(
        "SELECT `id`, `balance_cents`, `frozen_cents`, `status` FROM `wallet_account` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let (id, balance_cents, frozen_cents, status) = row.ok_or(WalletError::NotFound)?;
    Ok(WalletRow {
        id,
        balance_cents,
        frozen_cents,
        status: AccountStatus::parse(&status).unwrap_or(AccountStatus::Active),
    })
}

/// 变动钱包余额并记录 wallet_txn 流水：cents 为负出账、正入账，零值不动（旧版 Move）。
/// 出账要求 active 状态且可用余额充足，违约返回 Inactive / InsufficientBalance。
/// `w` 须为已持有的钱包锁；ref 为稳定流水及业务编号，note 为业务说明。
/// 流水业务类型与旧版一致：出账固定记 pay、入账固定记 refund（充值入账另走
/// credit::credit_recharge_in_tx，流水记 recharge）。成功后 `w.balance_cents`
/// 更新为写入值；失败后调用方必须回滚整个事务。
pub async fn move_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
    w: &mut WalletRow,
    cents: i64,
    ref_no: &str,
    note: &str,
) -> Result<(), WalletError> {
    if w.balance_cents < 0
        || w.frozen_cents < 0
        || w.frozen_cents > w.balance_cents
        || cents == i64::MIN
    {
        return Err(WalletError::InvalidBalance);
    }
    if cents > 0 && w.balance_cents > i64::MAX - cents {
        return Err(WalletError::InvalidBalance);
    }
    if cents < 0 && w.status != AccountStatus::Active {
        return Err(WalletError::Inactive);
    }
    if cents < 0 && w.balance_cents - w.frozen_cents < -cents {
        return Err(WalletError::InsufficientBalance);
    }
    if cents == 0 {
        return Ok(());
    }
    w.balance_cents += cents;
    let (direction, biz, amount) = if cents < 0 {
        (TxnDirection::Out, TxnBizType::Pay, -cents)
    } else {
        (TxnDirection::In, TxnBizType::Refund, cents)
    };
    let n = sqlx::query(
        "UPDATE `wallet_account` SET `balance_cents` = ?, `version` = `version` + 1 WHERE `id` = ?",
    )
    .bind(w.balance_cents)
    .bind(w.id as i64)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(WalletError::Storage("wallet row update lost".into()));
    }
    let month = match chrono::Utc::now().date_naive().with_day(1) {
        Some(d) => d,
        None => panic!("月首日必然存在"),
    };
    sqlx::query(
        "INSERT INTO `wallet_txn` (`txn_no`, `user_id`, `wallet_account_id`, `direction`, \
         `amount_cents`, `balance_after_cents`, `biz_type`, `biz_ref`, `note`, `created_month`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ref_no)
    .bind(user_id as i64)
    .bind(w.id as i64)
    .bind(direction.as_str())
    .bind(amount)
    .bind(w.balance_cents)
    .bind(biz.as_str())
    .bind(ref_no)
    .bind(note)
    .bind(month)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

use chrono::Datelike as _;
