//! 退款执行器（旧版 worker/refund/dispatch.go 的 simulation 通道对应物）。
//! 领取 pending/automatic 到期退款 → 渠道执行（simulation 直接成功）→
//! 支付单 refunded_cents 推进（CHECK 约束保护）→ 订单 refunding→refunded。
//! 真实渠道执行随批 6 微信封装接入；claimed 领取防并发重复执行。SQL 在 repo/。

use crate::repo::{refund_executor as repo, set_read_committed};
use chargepilot_domain::money::{BizType, PaymentOrderStatus};
use sqlx::{Acquire, MySqlPool};

/// 执行一轮到期自动退款，返回成功笔数。
pub async fn run_once(pool: &MySqlPool, limit: u32) -> anyhow::Result<usize> {
    let rows = repo::due_automatic_refunds(pool, limit).await?;
    let mut ok = 0usize;
    for (refund_id, refund_cents, payment_order_id, user_id, biz_id, biz_type) in rows {
        match execute_one(
            pool,
            refund_id,
            refund_cents,
            payment_order_id,
            user_id,
            biz_id,
            biz_type,
        )
        .await
        {
            Ok(true) => ok += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(refund_id, error = %e, "退款执行失败");
                repo::retry_later(pool, refund_id).await?;
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
    biz_type: BizType,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 锁退款记录并核对状态（pending → processing 条件更新防并发重复执行）。
    let marked = repo::claim_refund_tx(&mut *c, refund_id).await?;
    if marked != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    // 锁支付单（资金推进必须持支付锁；锁序：退款 → 支付，与预占的 钱包→支付 不同路径）。
    let Some((paid_cents, refunded_cents, pay_status)) =
        repo::lock_pay_facts_tx(&mut *c, payment_order_id).await?
    else {
        tx.rollback().await?;
        repo::fail_refund(pool, refund_id, "支付单不存在").await?;
        return Ok(true);
    };
    if !matches!(
        pay_status,
        PaymentOrderStatus::Paid | PaymentOrderStatus::PartialRefunded
    ) {
        tx.rollback().await?;
        repo::fail_refund(pool, refund_id, "支付单状态不允许退款").await?;
        return Ok(true);
    }
    if refunded_cents + refund_cents > paid_cents {
        tx.rollback().await?;
        repo::fail_refund(pool, refund_id, "退款超出可退余额").await?;
        return Ok(true);
    }
    // 渠道执行：simulation 直接成功（真实渠道随批 6；未知结果走 retry_later）。
    let new_refunded = refunded_cents + refund_cents;
    let new_status = if new_refunded >= paid_cents {
        PaymentOrderStatus::Refunded
    } else {
        PaymentOrderStatus::PartialRefunded
    };
    let advanced = repo::advance_refunded_tx(
        &mut *c,
        payment_order_id,
        new_refunded,
        new_status,
        refunded_cents,
        pay_status,
    )
    .await?;
    if advanced != 1 {
        tx.rollback().await?;
        repo::retry_later(pool, refund_id).await?;
        return Ok(false);
    }
    // 退款记录成功。
    repo::complete_refund_tx(&mut *c, refund_id).await?;
    // 充值退款（wallet_recharge）：simulation 无原路，金额直接回钱包余额。
    if biz_type == BizType::WalletRecharge {
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
    // 证据链（票 14）：读取行内状态分派证据，refunding → refunded（迁移 5）；
    // 行不存在或状态非 refunding 时不迁移不更新——保留旧版条件 UPDATE 静默
    // 0 行语义（后续事件照常登记，与原行为一致）。
    if biz_type == BizType::Charge {
        let order_status = repo::order_status_tx(&mut *c, biz_id).await?;
        let refunding = order_status
            .and_then(|st| {
                chargepilot_domain::charging::state::evidence_of(
                    chargepilot_domain::charging::state::OrderId(biz_id),
                    st,
                )
            })
            .and_then(chargepilot_domain::charging::state::StateEvidence::into_refunding);
        if refunding.is_some() {
            // 证据链：refunding → refunded（迁移 5）。
            repo::mark_order_refunded_tx(&mut *c, biz_id, payment_order_id).await?;
        }
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
