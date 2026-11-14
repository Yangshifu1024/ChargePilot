//! 充值到账：支付回调所需的申请锁定与钱包入账（旧版 recharge_credit_repo.go 对齐）。
//! 调用前须锁支付与充值申请；重复通知由支付回调幂等表提前识别，不会二次入账。

use super::WalletError;
use sqlx::MySqlConnection;

/// 支付回调所需的充值申请查询结果（旧版 RechargeReceipt）。
#[derive(Debug, Clone)]
pub struct RechargeReceipt {
    pub request_id: String,
    pub user_id: u64,
    pub amount_cents: i64,
    /// 所属用户的固定微信身份，用于核对渠道付款人。
    pub openid: String,
}

/// 在调用方事务中锁定 paymentID 的充值申请；无申请返回 NotFound。
/// 支付回调必须先锁支付行再调用；锁序为支付→申请→钱包（旧版 LockRechargeForPayment）。
pub async fn lock_recharge_for_payment_tx(
    tx: &mut MySqlConnection,
    payment_id: u64,
) -> Result<RechargeReceipt, WalletError> {
    // request_id 列为 char(36) ascii_bin（sqlx 视作 BINARY），读取侧 CAST 为 VARCHAR。
    let row: Option<(String, u64, i64)> = sqlx::query_as(
        "SELECT CAST(`request_id` AS CHAR), `user_id`, `amount_cents` FROM `wallet_recharge_request` \
         WHERE `payment_order_id` = ? LIMIT 1 FOR UPDATE",
    )
    .bind(payment_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let (request_id, user_id, amount_cents) = row.ok_or(WalletError::NotFound)?;
    // openid 是登录身份的固定标识，不由用户修改。只读，不反向取得用户行锁。
    let openid = super::account::find_user_openid_tx(tx, user_id).await?;
    Ok(RechargeReceipt {
        request_id,
        user_id,
        amount_cents,
        openid,
    })
}

/// 将已确认金额（分）计入钱包，并保存申请关联流水（旧版 CreditRechargeInTx）。
/// digest 是至少 32 字符的验签通知摘要（作流水号与业务引用）。
/// 余额、版本与流水同事务，失败全部回滚；钱包冻结不拒收到账资金。
pub async fn credit_recharge_in_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
    request_id: &str,
    digest: &str,
    amount_cents: i64,
) -> Result<(), WalletError> {
    if amount_cents <= 0 || digest.len() < 32 {
        return Err(WalletError::InvalidBalance);
    }
    let row: Option<(u64, i64, i64, u64)> = sqlx::query_as(
        "SELECT `id`, `balance_cents`, `frozen_cents`, `version` FROM `wallet_account` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let (wallet_id, balance, _frozen, version) = row.ok_or(WalletError::NotFound)?;
    if balance > i64::MAX - amount_cents {
        return Err(WalletError::InvalidBalance);
    }
    let new_balance = balance + amount_cents;
    // 乐观版本号条件更新：锁内读取后仍以 version 校验，防御绕锁写入。
    let n = sqlx::query(
        "UPDATE `wallet_account` SET `balance_cents` = ?, `version` = ? \
         WHERE `id` = ? AND `version` = ?",
    )
    .bind(new_balance)
    .bind(version + 1)
    .bind(wallet_id)
    .bind(version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(WalletError::Storage(
            "recharge wallet version conflict".into(),
        ));
    }
    let txn_no = format!("RC{}", digest[..32].to_uppercase());
    let month = match chrono::Utc::now().date_naive().with_day(1) {
        Some(d) => d,
        None => panic!("月首日必然存在"),
    };
    sqlx::query(
        "INSERT INTO `wallet_txn` (`txn_no`, `user_id`, `wallet_account_id`, `direction`, \
         `amount_cents`, `balance_after_cents`, `biz_type`, `biz_ref`, `note`, `created_month`) \
         VALUES (?, ?, ?, 'in', ?, ?, 'recharge', ?, 'wechat recharge settled', ?)",
    )
    .bind(&txn_no)
    .bind(user_id as i64)
    .bind(wallet_id)
    .bind(amount_cents)
    .bind(new_balance)
    .bind(request_id)
    .bind(month)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

use chrono::Datelike as _;
