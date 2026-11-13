//! 钱包退款所需的支付来源锁定（旧版 internal/payment/recharge_source_repo.go 对齐）。
//! 归属：payment_order 表读取归 payment；本实现是 wallet::RechargeSourceReader 的
//! 唯一生产实现（cmd 装配注入）。
//!
//! 锁定策略（死锁防御，逐字对齐旧版）：先不加锁读取已支付候选 ID，再用主键
//! 按 ID 锁定并复核状态。不能直接按支付状态扫描加锁，否则可能锁到待支付记录，
//! 与到账事务形成死锁。

use chargepilot_domain::money::{PayMethod, PaymentBizType, PaymentStatus};
use chargepilot_mod_wallet::WalletError;
use chargepilot_mod_wallet::refund::RechargeSource;
use sqlx::MySqlConnection;

/// 充值来源读取器（旧版 RechargeSources）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RechargeSources;

/// 仍可发起渠道退款的支付单状态集合（旧版 refundablePaymentStatuses）。
fn refundable_statuses() -> [&'static str; 2] {
    [PaymentStatus::PAID, PaymentStatus::PARTIAL_REFUNDED]
}

impl chargepilot_mod_wallet::refund::RechargeSourceReader for RechargeSources {
    fn lock_recharge_sources<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        user_id: u64,
    ) -> chargepilot_mod_wallet::recharge::futures_bounds::BoxFuture<
        'a,
        Result<Vec<RechargeSource>, WalletError>,
    > {
        Box::pin(RechargeSources::lock_recharge_sources(self, tx, user_id))
    }
}

impl RechargeSources {
    /// 在已锁钱包的事务中，按支付主键递增锁定有效微信充值。
    /// 无来源只查询一次，有候选时再查询并加锁；调用方负责提交或回滚。
    pub async fn lock_recharge_sources(
        &self,
        tx: &mut MySqlConnection,
        user_id: u64,
    ) -> Result<Vec<RechargeSource>, WalletError> {
        // 先无锁读候选 ID。
        let ids: Vec<u64> = sqlx::query_scalar(
            "SELECT `id` FROM `payment_order` \
             WHERE `user_id` = ? AND `biz_type` = ? AND `pay_method` = ? \
               AND `status` IN (?, ?) AND `deleted_at` IS NULL ORDER BY `id`",
        )
        .bind(user_id as i64)
        .bind(PaymentBizType::WALLET_RECHARGE)
        .bind(PayMethod::WECHAT)
        .bind(refundable_statuses()[0])
        .bind(refundable_statuses()[1])
        .fetch_all(&mut *tx)
        .await?;
        let mut out = Vec::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(out);
        }
        // 主键逐个锁定并复核状态（与旧版 FORCE INDEX (PRIMARY) + IN 等价）。
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT `id`, `biz_id`, `paid_cents`, `total_cents`, `refunded_cents`, \
                    `wechat_transaction_id` \
             FROM `payment_order` \
             WHERE `id` IN ({placeholders}) AND `user_id` = ? AND `biz_type` = ? \
               AND `pay_method` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL \
             ORDER BY `id` FOR UPDATE"
        );
        let mut q = sqlx::query_as::<_, (u64, u64, i64, i64, i64, Option<String>)>(&sql);
        for id in &ids {
            q = q.bind(id);
        }
        q = q
            .bind(user_id as i64)
            .bind(PaymentBizType::WALLET_RECHARGE)
            .bind(PayMethod::WECHAT)
            .bind(refundable_statuses()[0])
            .bind(refundable_statuses()[1]);
        let rows = q.fetch_all(&mut *tx).await?;
        for (id, biz_id, paid, total, refunded, txn) in rows {
            out.push(RechargeSource {
                id,
                biz_id,
                paid_cents: paid,
                total_cents: total,
                refunded_cents: refunded,
                // 渠道交易号为空不可原路退款。
                transaction_confirmed: txn.is_some(),
            });
        }
        Ok(out)
    }
}
