//! 支付成功处理（旧版 CallbackService.Apply 对齐）：回调与模拟确认共用的
//! 状态机入口。READ COMMITTED 单事务完成支付锁定 → 幂等摘要 → 状态推进 →
//! （charge 业务）订单创建 + 快照冻结 + 事件出站；钱包充值走钱包入账。
//! 每一步幂等：重放同报文返回相同回执，不同报文报冲突。SQL 在 repo/。

use crate::error::ApplyError;
use crate::repo::{
    apply as repo, find_callback_digest, insert_callback_digest_tx, set_read_committed,
};
use chargepilot_domain::money::{BizType, PaymentOrderStatus};
use chargepilot_domain::payment::{PaymentIntentStatus, is_late_payment};
use sqlx::{Acquire, MySqlPool};

/// 已验签（或模拟确认）的支付事实（旧版 VerifiedPayment）。
#[derive(Debug, Clone)]
pub struct VerifiedPayment {
    /// 商户支付单号（P 前缀）。
    pub merchant_order_no: String,
    /// 渠道交易号（simulation 为 SIM 前缀稳定编号）。
    pub transaction_id: String,
    /// 付款人微信 openid（simulation 为空串，跳过核验）。
    pub open_id: String,
    /// 到账金额，分。
    pub paid_cents: i64,
    /// 到账时刻（UTC 微秒）。
    pub paid_at_unix_micros: i64,
}

/// 支付处理结果（旧版 PaymentCallbackResult）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ApplyResult {
    pub charge_order_id: u64,
    pub charge_order_no: String,
    /// 幂等重放。
    pub replayed: bool,
    /// 迟到支付：只登记退款，不启动充电。
    pub refund_required: bool,
}

/// 通知内容摘要（旧版 callbackDigest）：对关键字段拼接后 sha256。
fn payment_digest(payment: &VerifiedPayment) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(payment.merchant_order_no.as_bytes());
    hasher.update(b"|");
    hasher.update(payment.transaction_id.as_bytes());
    hasher.update(b"|");
    hasher.update(payment.open_id.as_bytes());
    hasher.update(b"|");
    hasher.update(payment.paid_cents.to_le_bytes());
    hasher.update(b"|");
    hasher.update(payment.paid_at_unix_micros.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// 支付成功处理（旧版 Apply；charge 与 wallet_recharge 分流共用）。
#[allow(clippy::too_many_lines)]
pub async fn apply_payment(
    pool: &MySqlPool,
    payment: &VerifiedPayment,
) -> Result<ApplyResult, ApplyError> {
    let paid_at =
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(payment.paid_at_unix_micros)
            .ok_or(ApplyError::Conflict)?;
    if payment.merchant_order_no.is_empty()
        || payment.transaction_id.is_empty()
        || payment.paid_cents <= 0
    {
        return Err(ApplyError::Conflict);
    }
    let digest = payment_digest(payment);
    let now = chrono::Utc::now();

    // 先设隔离级别再开事务（SET 在事务内执行会被 MySQL 拒绝）。
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let conn = tx.acquire().await?;

    // 先按渠道商户单号锁定支付订单，再按业务类型分流。
    let Some((
        order_id,
        biz_type,
        biz_id,
        order_user,
        total_cents,
        order_status,
        existing_txn,
        _month,
    )) = repo::lock_pay_order_tx(&mut *conn, &payment.merchant_order_no).await?
    else {
        return Err(ApplyError::Conflict);
    };

    if biz_type == BizType::WalletRecharge {
        // 钱包充值分支（旧版 settleWalletRecharge）：支付单推进 paid 后
        // 经钱包入账事务落余额与流水；摘要即流水号。
        let previous = find_callback_digest(&mut *conn, &payment.transaction_id).await?;
        if let Some(previous_digest) = &previous
            && previous_digest != &digest
        {
            return Err(ApplyError::Conflict);
        }
        if order_status == PaymentOrderStatus::Paid {
            if previous.is_none() {
                return Err(ApplyError::Conflict);
            }
            tx.commit().await?;
            return Ok(ApplyResult {
                replayed: true,
                ..Default::default()
            });
        }
        if order_status != PaymentOrderStatus::Initiated
            || previous.is_some()
            || total_cents != payment.paid_cents
        {
            return Err(ApplyError::Conflict);
        }
        insert_callback_digest_tx(&mut *conn, &payment.transaction_id, &digest).await?;
        let updated = repo::mark_wallet_order_paid_tx(
            &mut *conn,
            order_id,
            payment.paid_cents,
            &payment.transaction_id,
            paid_at,
        )
        .await?;
        if updated != 1 {
            return Err(ApplyError::Conflict);
        }
        let request_id = repo::recharge_request_id_tx(&mut *conn, order_id)
            .await?
            .unwrap_or_default();
        if request_id.is_empty() {
            return Err(ApplyError::Conflict);
        }
        chargepilot_mod_wallet::credit::credit_recharge_in_tx(
            &mut *conn,
            order_user,
            &request_id,
            &digest,
            payment.paid_cents,
        )
        .await?;
        tx.commit().await?;
        return Ok(ApplyResult::default());
    }
    if biz_type != BizType::Charge {
        return Err(ApplyError::Conflict);
    }

    // 锁定支付意图并核对归属、付款人与金额。
    let Some((
        intent_id,
        intent_user,
        intent_payment_id,
        intent_openid,
        _e_cents,
        _s_cents,
        charge_mode,
        charge_quantity,
        intent_total,
        port_code,
        intent_order_id,
        expires_at,
        intent_coupon_grant_id,
        intent_discount_cents,
    )) = repo::lock_intent_tx(&mut *conn, &payment.merchant_order_no).await?
    else {
        return Err(ApplyError::Conflict);
    };
    if intent_payment_id != order_id
        || order_user != intent_user
        || (!payment.open_id.is_empty() && intent_openid != payment.open_id)
        || total_cents != intent_total
        || total_cents != payment.paid_cents
        || charge_quantity == 0
    {
        return Err(ApplyError::Conflict);
    }

    // 回调幂等：同渠道交易号必须同摘要（旧版 PaymentCallbackDigestRecord）。
    let previous = find_callback_digest(&mut *conn, &payment.transaction_id).await?;
    if let Some(previous_digest) = &previous
        && previous_digest != &digest
    {
        return Err(ApplyError::Conflict);
    }

    let paid_status_like = order_status == PaymentOrderStatus::Paid
        || order_status == PaymentOrderStatus::PartialRefunded
        || order_status == PaymentOrderStatus::Refunded;
    if paid_status_like {
        // 已到账重放：必须与首次报文一致（交易号、金额、摘要已核对）。
        let txn_matches = existing_txn.as_deref() == Some(payment.transaction_id.as_str());
        if previous.is_none() || !txn_matches {
            return Err(ApplyError::Conflict);
        }
        // 迟到退款登记后的重放：意图仍 refund_required → 原样返回。
        if intent_order_id.is_none() {
            let current_status = repo::intent_status_tx(&mut *conn, &intent_id).await?;
            if current_status == PaymentIntentStatus::RefundRequired {
                tx.commit().await?;
                return Ok(ApplyResult {
                    refund_required: true,
                    replayed: true,
                    ..Default::default()
                });
            }
            return Err(ApplyError::Conflict);
        }
        let charge_order_id = biz_id;
        let order_no = repo::charge_order_no_tx(&mut *conn, charge_order_id, order_id).await?;
        tx.commit().await?;
        return Ok(ApplyResult {
            charge_order_id,
            charge_order_no: order_no,
            replayed: true,
            refund_required: false,
        });
    }
    // 首次处理：必须 initiated 且未关联业务、摘要未登记。
    if order_status != PaymentOrderStatus::Initiated || biz_id != 0 || previous.is_some() {
        return Err(ApplyError::Conflict);
    }
    insert_callback_digest_tx(&mut *conn, &payment.transaction_id, &digest).await?;

    // 条件更新到 paid：并发竞争失败即冲突（旧版 RowsAffected == 1）。
    let updated = repo::mark_charge_order_paid_tx(
        &mut *conn,
        order_id,
        &payment.merchant_order_no,
        payment.paid_cents,
        &payment.transaction_id,
        paid_at,
    )
    .await?;
    if updated != 1 {
        return Err(ApplyError::Conflict);
    }

    // 迟到支付：登记自动退款与出站事件，不启动充电（旧版 queueLatePaymentRefund）。
    let late = is_late_payment(
        now.timestamp_micros(),
        payment.paid_at_unix_micros,
        expires_at.timestamp_micros(),
    );
    if late {
        // 意图 → refund_required；退款编号由摘要稳定派生，失败整体回滚。
        let intent_updated = repo::mark_intent_refund_required_tx(&mut *conn, &intent_id).await?;
        if intent_updated != 1 {
            return Err(ApplyError::Conflict);
        }
        let refund_no = late_refund_no(&digest);
        repo::insert_late_refund_tx(
            &mut *conn,
            &refund_no,
            order_id,
            order_user,
            payment.paid_cents,
            "支付迟到，自动原路退款",
            chargepilot_mod_order::month_start_of(now),
        )
        .await?;
        let event_id = format!("P{}", &digest[..32.min(digest.len())]);
        chargepilot_events::enqueue::in_tx(
            &mut *conn,
            chargepilot_events::EventSource::User,
            chargepilot_events::EventKind::ChargePaymentLate,
            &event_id,
            &serde_json::json!({
                "event_id": event_id,
                "payment_order_id": order_id,
                "order_no": payment.merchant_order_no,
                "refund_no": refund_no,
            }),
        )
        .await
        .map_err(|e| ApplyError::Storage(e.to_string()))?;
        tx.commit().await?;
        return Ok(ApplyResult {
            refund_required: true,
            ..Default::default()
        });
    }

    // 支付确认到账后才核销优惠券（旧版 RedeemInTx：随资金事务一起提交或回滚；
    // 核销单号在充电行尚不存在时用 PENDING- 派生，重放沿用首次结果）。
    if intent_coupon_grant_id != 0 {
        chargepilot_mod_marketing::coupon::redeem_in_tx(
            &mut *conn,
            intent_user,
            intent_coupon_grant_id,
            order_id,
            intent_total,
            intent_discount_cents,
            &chargepilot_mod_marketing::coupon::pending_charge_no_for(order_id),
        )
        .await
        .map_err(|e| ApplyError::Storage(e.to_string()))?;
    }

    // 支付确认到账：创建充电订单（paid）并冻结快照（旧版首段）。
    let device_id = repo::intent_device_id_tx(&mut *conn, &intent_id).await?;
    let port_no = repo::intent_port_no_tx(&mut *conn, &intent_id).await?;
    let charge_no = chargepilot_mod_order::new_charge_order_no_tx(
        &mut *conn,
        payment.paid_at_unix_micros,
        &device_id,
        port_no,
    )
    .await
    .map_err(|e| match e {
        chargepilot_mod_order::OrderError::NumberConflict => ApplyError::Conflict,
        other => ApplyError::Storage(other.to_string()),
    })?;
    let draft = chargepilot_mod_order::PaidOrderDraft {
        order_no: charge_no.clone(),
        user_id: intent_user,
        device_id: device_id.clone(),
        port_no,
        port_code,
        payment_order_id: order_id,
        charge_mode,
        charge_quantity,
        discount_cents: intent_discount_cents,
    };
    let charge_order_id = chargepilot_mod_order::create_paid_in_tx(&mut *conn, &draft)
        .await
        .map_err(|e| ApplyError::Storage(e.to_string()))?;
    let snapshot_json = repo::intent_snapshot_tx(&mut *conn, &intent_id).await?;
    chargepilot_mod_order::freeze_pricing_in_tx(
        &mut *conn,
        charge_order_id,
        &intent_id,
        intent_user,
        &draft.port_code,
        &serde_json::from_str::<serde_json::Value>(&snapshot_json)
            .map_err(|e| ApplyError::Storage(e.to_string()))?,
    )
    .await
    .map_err(|e| ApplyError::Storage(e.to_string()))?;
    // 绑定业务 ID 与意图（条件更新，RowsAffected == 1）。
    let bound = repo::bind_charge_order_tx(&mut *conn, order_id, charge_order_id).await?;
    if bound != 1 {
        return Err(ApplyError::Conflict);
    }
    let paid = repo::mark_intent_paid_tx(&mut *conn, &intent_id, paid_at, charge_order_id).await?;
    if paid != 1 {
        return Err(ApplyError::Conflict);
    }

    // 订单事件 + 出站事件（事件驱动状态机的入口信号；批 4 的投递器消费）。
    let event_id = format!("P{}", &digest[..32.min(digest.len())]);
    chargepilot_mod_order::append_event_in_tx(
        &mut *conn,
        charge_order_id,
        &event_id,
        "payment_confirmed",
        "payment_callback",
        "verified payment callback",
        payment.paid_at_unix_micros,
    )
    .await
    .map_err(|e| ApplyError::Storage(e.to_string()))?;
    chargepilot_events::enqueue::in_tx(
        &mut *conn,
        chargepilot_events::EventSource::User,
        chargepilot_events::EventKind::ChargePaymentConfirmed,
        &event_id,
        &serde_json::json!({
            "event_id": event_id,
            "charge_order_id": charge_order_id,
            "order_no": charge_no,
            "payment_order_id": order_id,
        }),
    )
    .await
    .map_err(|e| ApplyError::Storage(e.to_string()))?;
    tx.commit().await?;
    Ok(ApplyResult {
        charge_order_id,
        charge_order_no: charge_no,
        replayed: false,
        refund_required: false,
    })
}

/// 迟到支付退款编号（旧版 stableRefundNo：WR 前缀 + 摘要前 16 字节 hex）。
fn late_refund_no(digest: &str) -> String {
    format!("WR{}", &digest[..32.min(digest.len())])
}
