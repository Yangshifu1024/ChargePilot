//! 退款执行器（旧版 worker/refund/dispatch.go 的 simulation 通道对应物）。
//! 领取 pending/automatic 到期退款 → 渠道执行（simulation 直接成功）→
//! 支付单 refunded_cents 推进（CHECK 约束保护）→ 订单 refunding→refunded。
//! 真实渠道执行随批 6 微信封装接入；claimed 领取防并发重复执行。

use chargepilot_domain::money::{PaymentBizType, PaymentStatus, RefundStatus};
use sqlx::{Acquire, MySqlPool};

/// 执行一轮到期自动退款，返回成功笔数。
pub async fn run_once(pool: &MySqlPool, limit: u32) -> anyhow::Result<usize> {
    // 领取：automatic、pending、到期（旧版 idx_refund_dispatch 路径）。
    let rows: Vec<(u64, i64, u64, u64, u64, String)> = sqlx::query_as(
        "SELECT r.`id`, r.`refund_cents`, r.`payment_order_id`, r.`user_id`, r.`biz_id`, r.`biz_type` \
         FROM `refund_record` r \
         WHERE r.`execution_policy` = 'automatic' AND r.`status` = ? \
           AND r.`next_attempt_at` <= UTC_TIMESTAMP(3) AND r.`deleted_at` IS NULL \
         ORDER BY r.`id` LIMIT ?",
    )
    .bind(RefundStatus::PENDING)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    let mut ok = 0usize;
    for (refund_id, refund_cents, payment_order_id, user_id, biz_id, biz_type) in rows {
        match execute_one(pool, refund_id, refund_cents, payment_order_id, user_id, biz_id, &biz_type).await {
            Ok(true) => ok += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(refund_id, error = %e, "退款执行失败");
                retry_later(pool, refund_id).await?;
            }
        }
    }
    Ok(ok)
}

/// 执行单笔退款。返回 Ok(true) 表示成功，Ok(false) 表示已被并发领取或状态不允许。
#[allow(clippy::too_many_arguments)]
async fn execute_one(
    pool: &MySqlPool,
    refund_id: u64,
    refund_cents: i64,
    payment_order_id: u64,
    user_id: u64,
    biz_id: u64,
    biz_type: &str,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 锁退款记录并核对状态（pending → processing 条件更新防并发重复执行）。
    let marked = sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `claimed_by` = 1, `claimed_at` = UTC_TIMESTAMP(3) \
         WHERE `id` = ? AND `status` = ? AND `execution_policy` = 'automatic'",
    )
    .bind(RefundStatus::PROCESSING)
    .bind(refund_id as i64)
    .bind(RefundStatus::PENDING)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if marked != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    // 锁支付单（资金推进必须持支付锁；锁序：退款 → 支付，与预占的 钱包→支付 不同路径）。
    let payment: Option<(i64, i64, String)> = sqlx::query_as(
        "SELECT `paid_cents`, `refunded_cents`, `status` FROM `payment_order` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((paid_cents, refunded_cents, pay_status)) = payment else {
        tx.rollback().await?;
        fail_refund(pool, refund_id, "支付单不存在").await?;
        return Ok(true);
    };
    if !matches!(
        pay_status.as_str(),
        PaymentStatus::PAID | PaymentStatus::PARTIAL_REFUNDED
    ) {
        tx.rollback().await?;
        fail_refund(pool, refund_id, "支付单状态不允许退款").await?;
        return Ok(true);
    }
    if refunded_cents + refund_cents > paid_cents {
        tx.rollback().await?;
        fail_refund(pool, refund_id, "退款超出可退余额").await?;
        return Ok(true);
    }
    // 渠道执行：simulation 直接成功（真实渠道随批 6；未知结果走 retry_later）。
    let new_refunded = refunded_cents + refund_cents;
    let new_status = if new_refunded >= paid_cents {
        PaymentStatus::REFUNDED
    } else {
        PaymentStatus::PARTIAL_REFUNDED
    };
    let advanced = sqlx::query(
        "UPDATE `payment_order` SET `refunded_cents` = ?, `status` = ? \
         WHERE `id` = ? AND `refunded_cents` = ? AND `status` = ?",
    )
    .bind(new_refunded)
    .bind(new_status)
    .bind(payment_order_id as i64)
    .bind(refunded_cents)
    .bind(pay_status)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if advanced != 1 {
        tx.rollback().await?;
        retry_later(pool, refund_id).await?;
        return Ok(false);
    }
    // 退款记录成功。
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `completed_at` = UTC_TIMESTAMP(3), \
         `wechat_refund_id` = CONCAT('SIMRF', ?) WHERE `id` = ?",
    )
    .bind(RefundStatus::SUCCESS)
    .bind(refund_id as i64)
    .bind(refund_id as i64)
    .execute(&mut *c)
    .await?;
    // 充值退款（wallet_recharge）：simulation 无原路，金额直接回钱包余额。
    if biz_type == PaymentBizType::WALLET_RECHARGE {
        let mut wallet = chargepilot_mod_wallet::ledger::lock_tx(&mut *c, user_id).await?;
        chargepilot_mod_wallet::ledger::move_tx(
            &mut *c,
            user_id,
            &mut wallet,
            refund_cents,
            &format!("RF{refund_id:020}"),
            "充值退款回账",
        )
        .await?;
    }
    // 充电订单：refunding → refunded（余款退清；旧版 SetStatusRefunded 语义）。
    if biz_type == PaymentBizType::CHARGE {
        sqlx::query(
            "UPDATE `charge_order` SET `status` = 'refunded' \
             WHERE `id` = ? AND `status` = 'refunding' AND `payment_order_id` = ?",
        )
        .bind(biz_id as i64)
        .bind(payment_order_id as i64)
        .execute(&mut *c)
        .await?;
        let event_id = format!("RF{refund_id:020}");
        chargepilot_mod_order::append_event_in_tx(
            &mut *c,
            biz_id,
            &event_id,
            "refund_completed",
            "billing",
            &format!("refund_cents={refund_cents}"),
            chrono::Utc::now().timestamp_micros(),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        chargepilot_mod_order::append_domain_event_tx(
            &mut *c,
            &event_id,
            "user",
            "refund_completed",
            &serde_json::json!({
                "event_id": event_id,
                "charge_order_id": biz_id,
                "refund_id": refund_id,
                "refund_cents": refund_cents,
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }
    tx.commit().await?;
    Ok(true)
}

/// 渠道明确拒绝的退款失败登记（新事务；主事务已回滚）。
async fn fail_refund(pool: &MySqlPool, refund_id: u64, reason: &str) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, `failure_reason` = ?, `completed_at` = UTC_TIMESTAMP(3) \
         WHERE `id` = ? AND `status` = ?",
    )
    .bind(RefundStatus::FAILED)
    .bind(reason)
    .bind(refund_id as i64)
    .bind(RefundStatus::PROCESSING)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 退避重排（渠道未知结果：保留 processing 稍后重查，其余退回 pending 延迟重试）。
async fn retry_later(pool: &MySqlPool, refund_id: u64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE `refund_record` SET `status` = ?, \
         `next_attempt_at` = DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 2 MINUTE), `retry_count` = `retry_count` + 1 \
         WHERE `id` = ? AND `status` = ?",
    )
    .bind(RefundStatus::PENDING)
    .bind(refund_id as i64)
    .bind(RefundStatus::PROCESSING)
    .execute(pool)
    .await?;
    Ok(())
}
