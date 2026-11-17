//! 退款执行的表访问：refund_record / payment_order / charge_order（蓝图第五章 repo 层）。

use chargepilot_domain::money::{BizType, PaymentOrderStatus, RefundStatus};
use sqlx::{MySqlConnection, MySqlPool};

/// 到期自动退款领取列表（按 next_attempt_at 与 automatic 策略索引）。
pub type DueRefundRow = (u64, i64, u64, u64, u64, BizType);

/// 领取：automatic、pending、到期。
pub async fn due_automatic_refunds(
    pool: &MySqlPool,
    limit: u32,
) -> Result<Vec<DueRefundRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.`id`, r.`refund_cents`, r.`payment_order_id`, r.`user_id`, r.`biz_id`, r.`biz_type` \
         FROM `refund_record` r \
         WHERE r.`execution_policy` = 'automatic' AND r.`status` = ? \
           AND r.`next_attempt_at` <= UTC_TIMESTAMP(3) AND r.`deleted_at` IS NULL \
         ORDER BY r.`id` LIMIT ?",
    )
    .bind(RefundStatus::Pending)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
}

/// 退款记录领取（pending → processing 条件更新防并发重复执行，RowsAffected 交调用方）。
pub async fn claim_refund_tx(c: &mut MySqlConnection, refund_id: u64) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `claimed_by` = 1, `claimed_at` = UTC_TIMESTAMP(3) \
         WHERE `id` = ? AND `status` = ? AND `execution_policy` = 'automatic'",
    )
    .bind(RefundStatus::Processing)
    .bind(refund_id as i64)
    .bind(RefundStatus::Pending)
    .execute(&mut *c)
    .await
    .map(|r| r.rows_affected())
}

/// 锁支付单读资金事实（资金推进必须持支付锁；锁序：退款 → 支付）。
pub async fn lock_pay_facts_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<Option<(i64, i64, PaymentOrderStatus)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `paid_cents`, `refunded_cents`, `status` FROM `payment_order` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 支付单退款推进（条件更新防旧快照，RowsAffected 交调用方）。
pub async fn advance_refunded_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
    new_refunded: i64,
    new_status: PaymentOrderStatus,
    refunded_cents: i64,
    pay_status: PaymentOrderStatus,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `payment_order` SET `refunded_cents` = ?, `status` = ? \
         WHERE `id` = ? AND `refunded_cents` = ? AND `status` = ?",
    )
    .bind(new_refunded)
    .bind(new_status)
    .bind(payment_order_id as i64)
    .bind(refunded_cents)
    .bind(pay_status)
    .execute(&mut *c)
    .await
    .map(|r| r.rows_affected())
}

/// 退款记录成功落库（simulation 渠道回执编号）。
pub async fn complete_refund_tx(
    c: &mut MySqlConnection,
    refund_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `completed_at` = UTC_TIMESTAMP(3), \
         `wechat_refund_id` = CONCAT('SIMRF', ?) WHERE `id` = ?",
    )
    .bind(RefundStatus::Success)
    .bind(refund_id as i64)
    .bind(refund_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 渠道明确拒绝的退款失败登记（新事务；主事务已回滚）。
pub async fn fail_refund(
    pool: &MySqlPool,
    refund_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `failure_reason` = ?, `completed_at` = UTC_TIMESTAMP(3) \
         WHERE `id` = ? AND `status` = ?",
    )
    .bind(RefundStatus::Failed)
    .bind(reason)
    .bind(refund_id as i64)
    .bind(RefundStatus::Processing)
    .execute(pool)
    .await?;
    Ok(())
}

/// 退避重排（渠道未知结果：保留 processing 稍后重查，其余退回 pending 延迟重试）。
pub async fn retry_later(pool: &MySqlPool, refund_id: u64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, \
         `next_attempt_at` = DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 2 MINUTE), `retry_count` = `retry_count` + 1 \
         WHERE `id` = ? AND `status` = ?",
    )
    .bind(RefundStatus::Pending)
    .bind(refund_id as i64)
    .bind(RefundStatus::Processing)
    .execute(pool)
    .await?;
    Ok(())
}
