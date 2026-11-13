//! 支付成功处理（旧版 CallbackService.Apply 对齐）：回调与模拟确认共用的
//! 状态机入口。READ COMMITTED 单事务完成支付锁定 → 幂等摘要 → 状态推进 →
//! （charge 业务）订单创建 + 快照冻结 + 事件出站；钱包充值走钱包入账。
//! 每一步幂等：重放同报文返回相同回执，不同报文报冲突。

use chargepilot_domain::money::{PaymentBizType, PaymentStatus};
use chargepilot_domain::payment_intent::{IntentStatus, is_late_payment};
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

/// 支付处理错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// 报文与既有记录冲突或单据状态不允许（HTTP 409）。
    Conflict,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Conflict => f.write_str("支付通知冲突"),
            ApplyError::Storage(m) => write!(f, "支付处理存储错误: {m}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        ApplyError::Storage(e.to_string())
    }
}

impl From<chargepilot_mod_wallet::WalletError> for ApplyError {
    fn from(e: chargepilot_mod_wallet::WalletError) -> Self {
        match e {
            chargepilot_mod_wallet::WalletError::RequestConflict => ApplyError::Conflict,
            other => ApplyError::Storage(other.to_string()),
        }
    }
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let conn = tx.acquire().await?;

    // 先按渠道商户单号锁定支付订单，再按业务类型分流。
    let order: Option<(
        u64,
        String,
        u64,
        u64,
        i64,
        String,
        Option<String>,
        chrono::NaiveDate,
    )> = sqlx::query_as(
        "SELECT `id`, `biz_type`, `biz_id`, `user_id`, `total_cents`, `status`, \
                    `wechat_transaction_id`, `created_month` \
             FROM `payment_order` WHERE `order_no` = ? FOR UPDATE",
    )
    .bind(&payment.merchant_order_no)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((
        order_id,
        biz_type,
        biz_id,
        order_user,
        total_cents,
        order_status,
        existing_txn,
        _month,
    )) = order
    else {
        return Err(ApplyError::Conflict);
    };

    if biz_type == PaymentBizType::WALLET_RECHARGE {
        // 钱包充值分支（旧版 settleWalletRecharge）：支付单推进 paid 后
        // 经钱包入账事务落余额与流水；摘要即流水号。
        let previous: Option<(String,)> = sqlx::query_as(
            "SELECT `request_digest` FROM `payment_callback_idempotent` WHERE `wechat_transaction_id` = ?",
        )
        .bind(&payment.transaction_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((previous_digest,)) = &previous {
            if previous_digest != &digest {
                return Err(ApplyError::Conflict);
            }
        }
        if order_status == PaymentStatus::PAID {
            if previous.is_none() {
                return Err(ApplyError::Conflict);
            }
            tx.commit().await?;
            return Ok(ApplyResult {
                replayed: true,
                ..Default::default()
            });
        }
        if order_status != PaymentStatus::INITIATED
            || previous.is_some()
            || total_cents != payment.paid_cents
        {
            return Err(ApplyError::Conflict);
        }
        sqlx::query(
            "INSERT INTO `payment_callback_idempotent` (`wechat_transaction_id`, `request_digest`) VALUES (?, ?)",
        )
        .bind(&payment.transaction_id)
        .bind(&digest)
        .execute(&mut *conn)
        .await?;
        let updated = sqlx::query(
            "UPDATE `payment_order` SET `status` = ?, `paid_cents` = ?, \
             `wechat_transaction_id` = ?, `paid_at` = ? \
             WHERE `id` = ? AND `status` = ? AND `paid_cents` = 0",
        )
        .bind(PaymentStatus::PAID)
        .bind(payment.paid_cents)
        .bind(&payment.transaction_id)
        .bind(paid_at)
        .bind(order_id as i64)
        .bind(PaymentStatus::INITIATED)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(ApplyError::Conflict);
        }
        let request_id: String = sqlx::query_scalar(
            "SELECT CAST(`request_id` AS CHAR) FROM `wallet_recharge_request` \
             WHERE `payment_order_id` = ? LIMIT 1",
        )
        .bind(order_id as i64)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or_default();
        if request_id.is_empty() {
            return Err(ApplyError::Conflict);
        }
        chargepilot_mod_wallet::credit::credit_recharge_in_tx(
            &mut *conn,
            order_user as u64,
            &request_id,
            &digest,
            payment.paid_cents,
        )
        .await?;
        tx.commit().await?;
        return Ok(ApplyResult::default());
    }
    if biz_type != PaymentBizType::CHARGE {
        return Err(ApplyError::Conflict);
    }

    // 锁定支付意图并核对归属、付款人与金额。
    let intent: Option<(
        String,
        u64,
        u64,
        String,
        i64,
        i64,
        u8,
        u16,
        i64,
        String,
        Option<u64>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT CAST(`intent_id` AS CHAR), `user_id`, `payment_order_id`, CAST(`openid` AS CHAR), \
                    `electric_cents`, `service_cents`, `charge_mode`, `charge_quantity`, \
                    `total_cents`, `port_code`, `charge_order_id`, `expires_at` \
             FROM `charge_payment_intent` WHERE `merchant_order_no` = ? FOR UPDATE",
    )
    .bind(&payment.merchant_order_no)
    .fetch_optional(&mut *conn)
    .await?;
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
    )) = intent
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
    let previous: Option<(String,)> = sqlx::query_as(
        "SELECT `request_digest` FROM `payment_callback_idempotent` \
         WHERE `wechat_transaction_id` = ?",
    )
    .bind(&payment.transaction_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((previous_digest,)) = &previous {
        if previous_digest != &digest {
            return Err(ApplyError::Conflict);
        }
    }

    let paid_status_like = order_status == PaymentStatus::PAID
        || order_status == PaymentStatus::PARTIAL_REFUNDED
        || order_status == PaymentStatus::REFUNDED;
    if paid_status_like {
        // 已到账重放：必须与首次报文一致（交易号、金额、摘要已核对）。
        let txn_matches = existing_txn.as_deref() == Some(payment.transaction_id.as_str());
        if previous.is_none() || !txn_matches {
            return Err(ApplyError::Conflict);
        }
        // 迟到退款登记后的重放：意图仍 refund_required → 原样返回。
        if intent_order_id.is_none() {
            let current_status: String = sqlx::query_scalar(
                "SELECT `status` FROM `charge_payment_intent` WHERE `intent_id` = ?",
            )
            .bind(&intent_id)
            .fetch_one(&mut *conn)
            .await?;
            if current_status == IntentStatus::REFUND_REQUIRED {
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
        let order_no: String = sqlx::query_scalar(
            "SELECT `order_no` FROM `charge_order` WHERE `id` = ? AND `payment_order_id` = ?",
        )
        .bind(charge_order_id as i64)
        .bind(order_id as i64)
        .fetch_one(&mut *conn)
        .await?;
        tx.commit().await?;
        return Ok(ApplyResult {
            charge_order_id,
            charge_order_no: order_no,
            replayed: true,
            refund_required: false,
        });
    }
    // 首次处理：必须 initiated 且未关联业务、摘要未登记。
    if order_status != PaymentStatus::INITIATED || biz_id != 0 || previous.is_some() {
        return Err(ApplyError::Conflict);
    }
    sqlx::query(
        "INSERT INTO `payment_callback_idempotent` (`wechat_transaction_id`, `request_digest`) VALUES (?, ?)",
    )
    .bind(&payment.transaction_id)
    .bind(&digest)
    .execute(&mut *conn)
    .await?;

    // 条件更新到 paid：并发竞争失败即冲突（旧版 RowsAffected == 1）。
    let updated = sqlx::query(
        "UPDATE `payment_order` SET `status` = ?, `paid_cents` = ?, \
         `wechat_transaction_id` = ?, `paid_at` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ? AND `total_cents` = ? AND `paid_cents` = 0",
    )
    .bind(PaymentStatus::PAID)
    .bind(payment.paid_cents)
    .bind(&payment.transaction_id)
    .bind(paid_at)
    .bind(order_id as i64)
    .bind(&payment.merchant_order_no)
    .bind(PaymentStatus::INITIATED)
    .bind(payment.paid_cents)
    .execute(&mut *conn)
    .await?
    .rows_affected();
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
        let intent_updated = sqlx::query(
            "UPDATE `charge_payment_intent` SET `status` = ? \
             WHERE `intent_id` = ? AND `status` = ? AND `charge_order_id` IS NULL",
        )
        .bind(IntentStatus::REFUND_REQUIRED)
        .bind(&intent_id)
        .bind(IntentStatus::INITIATED)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if intent_updated != 1 {
            return Err(ApplyError::Conflict);
        }
        let refund_no = super::apply::late_refund_no(&digest);
        sqlx::query(
            "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
             `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
             VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, ?)",
        )
        .bind(&refund_no)
        .bind(order_id as i64)
        .bind(order_user)
        .bind(PaymentBizType::CHARGE)
        .bind(payment.paid_cents)
        .bind("支付迟到，自动原路退款")
        .bind(chargepilot_domain::money::RefundStatus::PENDING)
        .bind(chargepilot_domain::money::ExecutionPolicy::AUTOMATIC)
        .bind(chargepilot_mod_order::month_start_of(now))
        .execute(&mut *conn)
        .await?;
        let event_id = format!("P{}", &digest[..32.min(digest.len())]);
        chargepilot_mod_order::append_domain_event_tx(
            &mut *conn,
            &event_id,
            "user",
            "charge_payment_late",
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

    // 支付确认到账：创建充电订单（paid）并冻结快照（旧版首段；券核销阶段 05 接入）。
    let device_id: String =
        sqlx::query_scalar("SELECT `device_id` FROM `charge_payment_intent` WHERE `intent_id` = ?")
            .bind(&intent_id)
            .fetch_one(&mut *conn)
            .await?;
    let port_no: u8 =
        sqlx::query_scalar("SELECT `port_no` FROM `charge_payment_intent` WHERE `intent_id` = ?")
            .bind(&intent_id)
            .fetch_one(&mut *conn)
            .await?;
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
        user_id: intent_user as u64,
        device_id: device_id.clone(),
        port_no,
        port_code,
        payment_order_id: order_id,
        charge_mode,
        charge_quantity: charge_quantity as u16,
        discount_cents: 0,
    };
    let charge_order_id = chargepilot_mod_order::create_paid_in_tx(&mut *conn, &draft)
        .await
        .map_err(|e| ApplyError::Storage(e.to_string()))?;
    let snapshot_json: String = sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_payment_intent` WHERE `intent_id` = ?",
    )
    .bind(&intent_id)
    .fetch_one(&mut *conn)
    .await?;
    chargepilot_mod_order::freeze_pricing_in_tx(
        &mut *conn,
        charge_order_id,
        &intent_id,
        intent_user as u64,
        &draft.port_code,
        &serde_json::from_str::<serde_json::Value>(&snapshot_json)
            .map_err(|e| ApplyError::Storage(e.to_string()))?,
    )
    .await
    .map_err(|e| ApplyError::Storage(e.to_string()))?;
    // 绑定业务 ID 与意图（条件更新，RowsAffected == 1）。
    let bound = sqlx::query(
        "UPDATE `payment_order` SET `biz_id` = ? WHERE `id` = ? AND `biz_id` = 0 AND `status` = ?",
    )
    .bind(charge_order_id as i64)
    .bind(order_id as i64)
    .bind(PaymentStatus::PAID)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if bound != 1 {
        return Err(ApplyError::Conflict);
    }
    let paid = sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ?, `paid_at` = ?, `charge_order_id` = ? \
         WHERE `intent_id` = ? AND `status` = ? AND `charge_order_id` IS NULL",
    )
    .bind(IntentStatus::PAID)
    .bind(paid_at)
    .bind(charge_order_id as i64)
    .bind(&intent_id)
    .bind(IntentStatus::INITIATED)
    .execute(&mut *conn)
    .await?
    .rows_affected();
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
    chargepilot_mod_order::append_domain_event_tx(
        &mut *conn,
        &event_id,
        "user",
        "charge_payment_confirmed",
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
