//! 退款记录创建与在途聚合（旧版 internal/settlement/refund_creation_repo.go 对齐）。
//! 归属：refund_record 表写入归 settlement；本实现是 wallet::RefundRecorder 的
//! 唯一生产实现（cmd 装配注入）。所有退款创建路径都须遵守支付锁，避免旧快照遗漏预占。

use chargepilot_domain::money::{ExecutionPolicy, RefundDraft, RefundStatus};
use chargepilot_mod_wallet::WalletError;
use sqlx::MySqlConnection;

/// 退款记录写入器（旧版 RefundRecorder）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RefundRecorder;

impl chargepilot_mod_wallet::refund::RefundRecorder for RefundRecorder {
    fn reserved_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        payment_id: u64,
    ) -> chargepilot_mod_wallet::recharge::futures_bounds::BoxFuture<'a, Result<i64, WalletError>>
    {
        Box::pin(RefundRecorder::reserved_in_tx(self, tx, payment_id))
    }
    fn create_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        draft: RefundDraft,
    ) -> chargepilot_mod_wallet::recharge::futures_bounds::BoxFuture<'a, Result<u64, WalletError>>
    {
        Box::pin(RefundRecorder::create_in_tx(self, tx, draft))
    }
}

impl RefundRecorder {
    /// 在支付锁内用当前读合计在途退款，金额单位为分（旧版 ReservedInTx）。
    pub async fn reserved_in_tx(
        &self,
        tx: &mut MySqlConnection,
        payment_id: u64,
    ) -> Result<i64, WalletError> {
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT `refund_cents` FROM `refund_record` \
             WHERE `payment_order_id` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL \
             FOR UPDATE",
        )
        .bind(payment_id as i64)
        .bind(RefundStatus::PENDING)
        .bind(RefundStatus::PROCESSING)
        .fetch_all(&mut *tx)
        .await?;
        Ok(rows.iter().sum())
    }

    /// 登记 automatic/pending 退款并返回主键（旧版 CreateInTx）。
    /// 调用方先锁钱包（适用时）和支付，核对额度；错误整体回滚。
    pub async fn create_in_tx(
        &self,
        tx: &mut MySqlConnection,
        draft: RefundDraft,
    ) -> Result<u64, WalletError> {
        let id = sqlx::query(
            "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, \
             `biz_type`, `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, \
             `created_month`, `next_attempt_at`) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, CURRENT_TIMESTAMP(3)))",
        )
        .bind(&draft.refund_no)
        .bind(draft.payment_order_id as i64)
        .bind(draft.user_id as i64)
        .bind(&draft.biz_type.0)
        .bind(draft.biz_id as i64)
        .bind(draft.amount_cents)
        .bind(&draft.reason)
        .bind(RefundStatus::PENDING)
        .bind(ExecutionPolicy::AUTOMATIC)
        .bind(draft.created_month)
        .bind(draft.next_attempt_at)
        .execute(&mut *tx)
        .await?
        .last_insert_id() as u64;
        Ok(id)
    }
}
