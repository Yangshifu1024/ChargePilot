//! 人工退款：登记、双人审核与重试（旧版 refund_review_repo.go + refund.go 的
//! 资金核心）。权限校验与审计由 HTTP 层负责，本模块只做资金语义：
//! 锁序 支付 → 退款；审核通过后转 automatic 由退款执行器接手。
//! 已结束充电必须先有费用回执，不能抢在计费之前退款。

use chargepilot_domain::money::{
    ExecutionPolicy, PayMethod, PaymentBizType, PaymentStatus, RefundStatus,
};
use sqlx::{Acquire, MySqlPool};

/// 人工退款域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefundAdminError {
    /// 请求冲突：内容不一致、状态不允许或金额超限。
    Conflict,
    /// 已结束充电尚无费用回执。
    NotSettled,
    /// 退款单不存在。
    NotFound,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for RefundAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefundAdminError::Conflict => f.write_str("人工退款请求冲突"),
            RefundAdminError::NotSettled => f.write_str("实际结束的充电尚无费用回执"),
            RefundAdminError::NotFound => f.write_str("退款单不存在"),
            RefundAdminError::Storage(m) => write!(f, "人工退款存储错误: {m}"),
        }
    }
}

impl std::error::Error for RefundAdminError {}

impl From<sqlx::Error> for RefundAdminError {
    fn from(e: sqlx::Error) -> Self {
        RefundAdminError::Storage(e.to_string())
    }
}

/// 退款单号：MR + request_id 的 sha256 前 16 字节十六进制（旧版 manualRefundNo）。
fn manual_refund_no(request_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(request_id.as_bytes());
    let hex: String = sum[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("MR{hex}")
}

/// 登记人工退款；同 request_id 同内容重放返回首次退款号（created=false），
/// 内容不同返回冲突。金额不得超过可退余额；已结束充电须已有费用回执。
pub async fn request_manual_refund(
    pool: &MySqlPool,
    actor_id: u64,
    order_id: u64,
    request_id: &str,
    amount_cents: i64,
    reason: &str,
) -> Result<(bool, String), RefundAdminError> {
    if amount_cents <= 0 || reason.trim().is_empty() || reason.len() > 255 {
        return Err(RefundAdminError::Conflict);
    }
    let refund_no = manual_refund_no(request_id);
    let payload = serde_json::json!({
        "actor_id": actor_id, "order_id": order_id,
        "amount_cents": amount_cents, "reason": reason,
    })
    .to_string();

    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 订单 + 支付锁（支付 → 退款锁序）。
    let order: Option<(u64, u64, String, i64)> = sqlx::query_as(
        "SELECT `payment_order_id`, `user_id`, `status`, `started_at` IS NOT NULL \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((payment_order_id, order_user_id, order_status, has_started)) = order else {
        return Err(RefundAdminError::NotFound);
    };
    let payment: Option<(u64, i64, i64, String, String)> = sqlx::query_as(
        "SELECT `id`, `paid_cents`, `refunded_cents`, `status`, `pay_method` \
         FROM `payment_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((pay_id, paid_cents, refunded_cents, pay_status, pay_method)) = payment else {
        return Err(RefundAdminError::NotFound);
    };
    // 锁定读请求记录：重放比对首次快照（支付行锁不刷新旧快照）。
    let prior: Option<(String, String)> = sqlx::query_as(
        "SELECT CAST(`payload_json` AS CHAR) AS `payload_json`, CAST(`refund_no` AS CHAR) AS `refund_no` \
         FROM `manual_refund_request` WHERE CAST(`request_id` AS CHAR) = ? FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await?;
    if let Some((prior_payload, prior_no)) = prior {
        // json 列存储会规范化文本，按语义（Value）比较而非逐字。
        let same = serde_json::from_str::<serde_json::Value>(&prior_payload)
            .ok()
            .map(|v| v == serde_json::from_str::<serde_json::Value>(&payload).unwrap_or_default())
            .unwrap_or(false);
        if !same {
            return Err(RefundAdminError::Conflict);
        }
        tx.commit().await?;
        return Ok((false, prior_no));
    }
    // 准入：订单可退（旧版 AllChannelRefundableStatuses：completed/cancelled/failed，
    // refunded 被随后单独排除；refunding 不在集合——已受理退款不得重复发起）。
    let refundable_order = matches!(order_status.as_str(), "completed" | "cancelled" | "failed");
    let refundable_pay = matches!(
        pay_status.as_str(),
        PaymentStatus::PAID | PaymentStatus::PARTIAL_REFUNDED
    );
    if !refundable_order
        || order_status == "refunded"
        || !refundable_pay
        || pay_method != PayMethod::WECHAT
        || amount_cents > paid_cents - refunded_cents
    {
        return Err(RefundAdminError::Conflict);
    }
    // 已结束的真实充电必须先入账（启动失败/未启动订单保留原退款准入）。
    if has_started != 0
        && matches!(
            order_status.as_str(),
            "completed" | "refunding" | "refunded"
        )
    {
        let receipt: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM `charge_fee_receipt` WHERE `charge_order_id` = ?")
                .bind(order_id as i64)
                .fetch_optional(&mut *c)
                .await?;
        if receipt.is_none() {
            return Err(RefundAdminError::NotSettled);
        }
    }
    // 待退聚合（当前读，纳入锁等待期间提交的预占）。
    let pending: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(`refund_cents`), 0) AS SIGNED) FROM `refund_record` \
         WHERE `payment_order_id` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(pay_id as i64)
    .bind(RefundStatus::PENDING)
    .bind(RefundStatus::PROCESSING)
    .fetch_one(&mut *c)
    .await?;
    if pending > paid_cents - refunded_cents - amount_cents {
        return Err(RefundAdminError::Conflict);
    }
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `next_attempt_at`, \
         `created_month`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3), ?)",
    )
    .bind(&refund_no)
    .bind(pay_id as i64)
    .bind(order_user_id as i64)
    .bind(PaymentBizType::CHARGE)
    .bind(order_id as i64)
    .bind(amount_cents)
    .bind(reason)
    .bind(RefundStatus::PENDING)
    .bind(ExecutionPolicy::MANUAL_REVIEW)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&mut *c)
    .await?;
    sqlx::query(
        "INSERT INTO `manual_refund_request` (`request_id`, `payload_json`, `refund_no`) \
         VALUES (?, ?, ?)",
    )
    .bind(request_id)
    .bind(&payload)
    .bind(&refund_no)
    .execute(&mut *c)
    .await?;
    tx.commit().await?;
    Ok((true, refund_no))
}

/// 审核结果（旧版 RefundReviewOutcome）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// 第一审通过，等待第二审。
    AwaitingSecond,
    /// 双审通过，已转 automatic（退款执行器将接手）。
    Approved,
    /// 单人驳回。
    Rejected,
}

/// 审核/驳回人工退款（旧版 Review）：仅 pending + manual_review 可审。
/// 驳回写拒绝记录并置 rejected；通过走双人：首审存快照，二审核对快照与
/// 首审人权限（由调用方传入实时查询结果）后转 automatic。
pub async fn review_manual_refund(
    pool: &MySqlPool,
    actor_id: u64,
    refund_no: &str,
    approve: bool,
    approve_comment: &str,
    reason: &str,
    first_signer_permitted: Option<bool>,
) -> Result<ReviewOutcome, RefundAdminError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    let record: Option<(u64, u64, u64, i64, String, u64)> = sqlx::query_as(
        "SELECT `id`, `payment_order_id`, `user_id`, `refund_cents`, `biz_type`, `biz_id` \
         FROM `refund_record` WHERE CAST(`refund_no` AS CHAR) = ? AND `deleted_at` IS NULL",
    )
    .bind(refund_no)
    .fetch_optional(&mut *c)
    .await?;
    let Some((record_id, payment_order_id, user_id, refund_cents, biz_type, biz_id)) = record
    else {
        return Err(RefundAdminError::NotFound);
    };
    // 支付 → 退款锁序。
    let payment: Option<(u64, i64, i64, String, String)> = sqlx::query_as(
        "SELECT `id`, `paid_cents`, `refunded_cents`, `status`, `pay_method` \
         FROM `payment_order` WHERE `id` = ? FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((pay_id, paid_cents, refunded_cents, _pay_status, pay_method)) = payment else {
        return Err(RefundAdminError::NotFound);
    };
    let _ = pay_id;
    let state: Option<(String, String)> = sqlx::query_as(
        "SELECT `status`, `execution_policy` FROM `refund_record` WHERE `id` = ? FOR UPDATE",
    )
    .bind(record_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((status, policy)) = state else {
        return Err(RefundAdminError::NotFound);
    };
    if status != RefundStatus::PENDING || policy != ExecutionPolicy::MANUAL_REVIEW {
        return Err(RefundAdminError::Conflict);
    }
    // 驳回：单人即可，写拒绝记录。
    if !approve {
        sqlx::query(
            "INSERT INTO `refund_rejection` (`refund_record_id`, `actor_id`, `reason`) \
             VALUES (?, ?, ?)",
        )
        .bind(record_id as i64)
        .bind(actor_id as i64)
        .bind(reason)
        .execute(&mut *c)
        .await?;
        sqlx::query("UPDATE `refund_record` SET `status` = ?, `failure_reason` = ? WHERE `id` = ?")
            .bind(RefundStatus::REJECTED)
            .bind(reason)
            .bind(record_id as i64)
            .execute(&mut *c)
            .await?;
        tx.commit().await?;
        return Ok(ReviewOutcome::Rejected);
    }
    // 通过：金额与归属核对（审核期间可变状态不参与快照一致性）。
    if pay_method != PayMethod::WECHAT
        || refund_cents <= 0
        || refund_cents > paid_cents - refunded_cents
    {
        return Err(RefundAdminError::Conflict);
    }
    // 升级前已申请的人工退款同样不能在费用确认前批准。
    if biz_type == PaymentBizType::CHARGE {
        let (order_started, order_status): (i64, String) = sqlx::query_as(
            "SELECT `started_at` IS NOT NULL, `status` FROM `charge_order` WHERE `id` = ?",
        )
        .bind(biz_id as i64)
        .fetch_one(&mut *c)
        .await
        .map_err(|_| RefundAdminError::Conflict)?;
        if order_started != 0
            && matches!(
                order_status.as_str(),
                "completed" | "refunding" | "refunded"
            )
        {
            let receipt: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
            )
            .bind(biz_id as i64)
            .fetch_optional(&mut *c)
            .await?;
            if receipt.is_none() {
                return Err(RefundAdminError::NotSettled);
            }
        }
    }
    let snapshot = serde_json::json!({
        "refund_no": refund_no, "payment_order_id": payment_order_id, "user_id": user_id,
        "biz_type": biz_type, "biz_id": biz_id, "refund_cents": refund_cents,
    })
    .to_string();
    let existing: Option<(String, u64, Option<u64>)> = sqlx::query_as(
        "SELECT CAST(`snapshot_json` AS CHAR) AS `snapshot_json`, `first_signer`, `second_signer` \
         FROM `refund_review` WHERE `refund_record_id` = ? FOR UPDATE",
    )
    .bind(record_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let outcome = match existing {
        None => {
            // 首审：保存快照与首审人，等待第二审。
            sqlx::query(
                "INSERT INTO `refund_review` (`refund_record_id`, `snapshot_json`, \
                 `first_signer`, `first_comment`) VALUES (?, ?, ?, ?)",
            )
            .bind(record_id as i64)
            .bind(&snapshot)
            .bind(actor_id as i64)
            .bind(approve_comment)
            .execute(&mut *c)
            .await?;
            ReviewOutcome::AwaitingSecond
        }
        Some((stored_snapshot, first_signer, second_signer)) => {
            // 二审：须不同人、未二审、快照未变（json 列存储规范化后按语义比较）、
            // 首审人权限仍有效。
            let snapshot_same = serde_json::from_str::<serde_json::Value>(&stored_snapshot)
                .ok()
                .map(|v| {
                    v == serde_json::from_str::<serde_json::Value>(&snapshot).unwrap_or_default()
                })
                .unwrap_or(false);
            if second_signer.is_some() || first_signer == actor_id || !snapshot_same {
                return Err(RefundAdminError::Conflict);
            }
            if first_signer_permitted != Some(true) {
                return Err(RefundAdminError::Conflict);
            }
            sqlx::query(
                "UPDATE `refund_review` SET `second_signer` = ?, `second_comment` = ?, \
                 `approved_at` = UTC_TIMESTAMP(3) WHERE `refund_record_id` = ?",
            )
            .bind(actor_id as i64)
            .bind(approve_comment)
            .bind(record_id as i64)
            .execute(&mut *c)
            .await?;
            // 转 automatic 并立即排队：批 5 退款执行器接手渠道执行。
            sqlx::query(
                "UPDATE `refund_record` SET `execution_policy` = ?, \
                 `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
            )
            .bind(ExecutionPolicy::AUTOMATIC)
            .bind(record_id as i64)
            .execute(&mut *c)
            .await?;
            ReviewOutcome::Approved
        }
    };
    tx.commit().await?;
    Ok(outcome)
}

/// 重试 automatic 且未终态的退款：重置 next_attempt_at 立即排队。
/// 人工待审或终态返回冲突；金额与状态不变（旧版 Retry）。
pub async fn retry_refund(pool: &MySqlPool, refund_no: &str) -> Result<u64, RefundAdminError> {
    let mut conn = pool.acquire().await?;
    let record: Option<(u64, String, String)> = sqlx::query_as(
        "SELECT `id`, `execution_policy`, `status` FROM `refund_record` \
         WHERE CAST(`refund_no` AS CHAR) = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(refund_no)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((record_id, policy, status)) = record else {
        return Err(RefundAdminError::NotFound);
    };
    let unsettled = matches!(
        status.as_str(),
        RefundStatus::PENDING | RefundStatus::PROCESSING
    );
    if policy != ExecutionPolicy::AUTOMATIC || !unsettled {
        return Err(RefundAdminError::Conflict);
    }
    sqlx::query("UPDATE `refund_record` SET `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(record_id as i64)
        .execute(&mut *conn)
        .await?;
    Ok(record_id)
}
