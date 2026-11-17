//! 人工退款：登记、双人审核与重试。
//! 权限校验与审计由 HTTP 层负责，本模块只做资金语义：
//! 锁序 支付 → 退款；审核通过后转 automatic 由退款执行器接手。
//! 已结束充电必须先有费用回执，不能抢在计费之前退款。SQL 在 repo/refund_admin.rs。

use crate::repo::{refund_admin as repo, set_read_committed};
use chargepilot_domain::money::{ExecutionPolicy, PayMethod, PaymentOrderStatus, RefundStatus};
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::{Acquire, MySqlPool};

use crate::AuditView;
use crate::error::RefundAdminError;

pub use crate::repo::refund_admin::RefundAdminRow;

/// 分页倒序读取未删除退款及最近人工复核。
/// status / refund_no 为空表示不筛选；status 由调用方按白名单校验。
pub async fn list_refunds(
    db: &MySqlPool,
    status: &str,
    refund_no: &str,
    page: u32,
    size: u32,
) -> Result<(Vec<RefundAdminRow>, i64), anyhow::Error> {
    let (where_sql, binds) = repo::refund_filters(status, refund_no);
    let total = repo::refund_count(db, &where_sql, &binds).await?;
    let offset = (page.max(1) as i64 - 1) * size.max(1) as i64;
    let rows = repo::refund_rows(db, &where_sql, &binds, offset, size.max(1) as i64).await?;
    Ok((rows, total))
}

/// 退款单号：MR + request_id 的 sha256 前 16 字节十六进制。
fn manual_refund_no(request_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(request_id.as_bytes());
    let hex: String = sum[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("MR{hex}")
}

/// 登记人工退款；同 request_id 同内容重放返回首次退款号（created=false），
/// 内容不同返回冲突。金额不得超过可退余额；已结束充电须已有费用回执。
/// 首次登记时在事务内写审计（重放不写，created 分支语义）。
#[allow(clippy::too_many_lines)]
pub async fn request_manual_refund(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
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
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 订单 + 支付锁（支付 → 退款锁序）。
    let Some((payment_order_id, order_user_id, order_status, has_started)) =
        repo::lock_refund_order_tx(&mut *c, order_id).await?
    else {
        return Err(RefundAdminError::NotFound);
    };
    let Some((pay_id, paid_cents, refunded_cents, pay_status, pay_method)) =
        repo::lock_pay_facts_tx(&mut *c, payment_order_id).await?
    else {
        return Err(RefundAdminError::NotFound);
    };
    // 锁定读请求记录：重放比对首次快照（支付行锁不刷新旧快照）。
    if let Some((prior_payload, prior_no)) =
        repo::lock_manual_request_tx(&mut *c, request_id).await?
    {
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
    // 准入：订单 completed/cancelled/failed 且支付可退；refunding 不得重复登记。
    let refundable_order = matches!(
        order_status,
        ChargeOrderStatus::Completed | ChargeOrderStatus::Cancelled | ChargeOrderStatus::Failed
    );
    let refundable_pay = matches!(
        pay_status,
        PaymentOrderStatus::Paid | PaymentOrderStatus::PartialRefunded
    );
    if !refundable_order
        || order_status == ChargeOrderStatus::Refunded
        || !refundable_pay
        || pay_method != PayMethod::Wechat
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
        && !repo::fee_receipt_exists_tx(&mut *c, order_id).await?
    {
        return Err(RefundAdminError::NotSettled);
    }
    // 待退聚合（当前读，纳入锁等待期间提交的预占）。
    let pending = repo::pending_refund_sum_tx(&mut *c, pay_id).await?;
    if pending > paid_cents - refunded_cents - amount_cents {
        return Err(RefundAdminError::Conflict);
    }
    repo::insert_manual_refund_tx(
        &mut *c,
        &refund_no,
        pay_id,
        order_user_id,
        order_id,
        amount_cents,
        reason,
        chargepilot_mod_order::month_start_of(chrono::Utc::now()),
    )
    .await?;
    repo::insert_manual_request_tx(&mut *c, request_id, &payload, &refund_no).await?;
    let record_id = repo::last_insert_id_tx(&mut *c).await?;
    // 首次登记审计。
    if let Some(audit) = audit {
        let after = serde_json::json!({
            "request_id": request_id,
            "amount_cents": amount_cents,
            "reason": reason,
        });
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            audit.actor,
            "refund",
            "request",
            "refund",
            &record_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit.client_ip,
            request_id,
        )
        .await
        .map_err(|e| RefundAdminError::Storage(e.to_string()))?;
    }
    tx.commit().await?;
    Ok((true, refund_no))
}

/// 审核结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// 第一审通过，等待第二审。
    AwaitingSecond,
    /// 双审通过，已转 automatic（退款执行器将接手）。
    Approved,
    /// 单人驳回。
    Rejected,
}

/// 审核/驳回人工退款：仅 pending + manual_review 可审。
/// 驳回写拒绝记录并置 rejected；通过走双人：首审存快照，二审核对快照与
/// 首审人权限（由调用方传入实时查询结果）后转 automatic。
/// 决定落库时在事务内写审计（驳回与双审通过都写，service 层语义）。
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub async fn review_manual_refund(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    actor_id: u64,
    refund_no: &str,
    approve: bool,
    approve_comment: &str,
    reason: &str,
    first_signer_permitted: Option<bool>,
) -> Result<ReviewOutcome, RefundAdminError> {
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    let Some((record_id, payment_order_id, user_id, refund_cents, biz_type, biz_id)) =
        repo::find_refund_record_tx(&mut *c, refund_no).await?
    else {
        return Err(RefundAdminError::NotFound);
    };
    // 支付 → 退款锁序。
    let Some((pay_id, paid_cents, refunded_cents, _pay_status, pay_method)) =
        repo::lock_pay_facts_tx(&mut *c, payment_order_id).await?
    else {
        return Err(RefundAdminError::NotFound);
    };
    let _ = pay_id;
    let Some((status, policy)) = repo::lock_refund_state_tx(&mut *c, record_id).await? else {
        return Err(RefundAdminError::NotFound);
    };
    if status != RefundStatus::Pending || policy != ExecutionPolicy::ManualReview {
        return Err(RefundAdminError::Conflict);
    }
    // 驳回：单人即可，写拒绝记录。
    if !approve {
        repo::insert_rejection_tx(&mut *c, record_id, actor_id, reason).await?;
        repo::mark_refund_rejected_tx(&mut *c, record_id, reason).await?;
        write_refund_audit(
            &mut tx,
            audit,
            "rejected",
            record_id,
            approve_comment,
            reason,
        )
        .await?;
        tx.commit().await?;
        return Ok(ReviewOutcome::Rejected);
    }
    // 通过：金额与归属核对（审核期间可变状态不参与快照一致性）。
    if pay_method != PayMethod::Wechat
        || refund_cents <= 0
        || refund_cents > paid_cents - refunded_cents
    {
        return Err(RefundAdminError::Conflict);
    }
    // 升级前已申请的人工退款同样不能在费用确认前批准。
    if biz_type == chargepilot_domain::money::BizType::Charge {
        let (order_started, order_status) = repo::order_started_status_tx(&mut *c, biz_id)
            .await
            .map_err(|_| RefundAdminError::Conflict)?;
        if order_started != 0
            && matches!(
                order_status,
                ChargeOrderStatus::Completed
                    | ChargeOrderStatus::Refunding
                    | ChargeOrderStatus::Refunded
            )
            && !repo::fee_receipt_exists_tx(&mut *c, biz_id).await?
        {
            return Err(RefundAdminError::NotSettled);
        }
    }
    let snapshot = serde_json::json!({
        "refund_no": refund_no, "payment_order_id": payment_order_id, "user_id": user_id,
        "biz_type": biz_type, "biz_id": biz_id, "refund_cents": refund_cents,
    })
    .to_string();
    let existing = repo::lock_review_tx(&mut *c, record_id).await?;
    let outcome = match existing {
        None => {
            // 首审：保存快照与首审人，等待第二审。
            repo::insert_first_review_tx(&mut *c, record_id, &snapshot, actor_id, approve_comment)
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
            // 首审人当前权限：None 由事务内一致读取实时查询（HTTP 语义），
            // Some(v) 为调用方直接注入（模块级测试）。
            let permitted = match first_signer_permitted {
                Some(v) => v,
                None => {
                    let profile = chargepilot_mod_ops::repo::load_profile_tx(&mut *c, first_signer)
                        .await
                        .map_err(|_| RefundAdminError::Conflict)?;
                    profile.role == "customer_finance"
                        && profile
                            .permissions
                            .iter()
                            .any(|p| p == chargepilot_mod_ops::perms::ORDER_REFUND_REVIEW)
                }
            };
            if !permitted {
                return Err(RefundAdminError::Conflict);
            }
            repo::approve_second_review_tx(&mut *c, record_id, actor_id, approve_comment).await?;
            // 转 automatic 并立即排队：。
            repo::promote_to_automatic_tx(&mut *c, record_id).await?;
            ReviewOutcome::Approved
        }
    };
    // 决定审计。
    let action = match outcome {
        ReviewOutcome::Rejected => "rejected",
        ReviewOutcome::AwaitingSecond => "awaiting_second",
        ReviewOutcome::Approved => "approved",
    };
    write_refund_audit(&mut tx, audit, action, record_id, approve_comment, reason).await?;
    tx.commit().await?;
    Ok(outcome)
}

/// 退款决定的审计写入。
async fn write_refund_audit(
    tx: &mut sqlx::MySqlConnection,
    audit: Option<&AuditView<'_>>,
    action: &str,
    record_id: u64,
    approve_comment: &str,
    reason: &str,
) -> Result<(), RefundAdminError> {
    if let Some(audit) = audit {
        let after = serde_json::json!({
            "approve_comment": approve_comment,
            "reason": reason,
        });
        chargepilot_mod_ops::repo::append_audit_tx(
            tx,
            audit.actor,
            "refund",
            action,
            "refund",
            &record_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit.client_ip,
            audit.request_id,
        )
        .await
        .map_err(|e| RefundAdminError::Storage(e.to_string()))?;
    }
    Ok(())
}

/// 重试 automatic 且未终态的退款：重置 next_attempt_at 立即排队。
/// 人工待审或终态返回冲突；金额与状态不变。重复重试静默成功并写审计。
pub async fn retry_refund(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    refund_no: &str,
    reason: &str,
) -> Result<u64, RefundAdminError> {
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some((record_id, policy, status)) = repo::lock_retry_record_tx(&mut *c, refund_no).await?
    else {
        return Err(RefundAdminError::NotFound);
    };
    let unsettled = matches!(status, RefundStatus::Pending | RefundStatus::Processing);
    if policy != ExecutionPolicy::Automatic || !unsettled {
        return Err(RefundAdminError::Conflict);
    }
    repo::reset_next_attempt_tx(&mut *c, record_id).await?;
    // 重试审计。
    if let Some(audit) = audit {
        let after = serde_json::json!({ "reason": reason });
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            audit.actor,
            "refund",
            "retry",
            "refund",
            &record_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit.client_ip,
            audit.request_id,
        )
        .await
        .map_err(|e| RefundAdminError::Storage(e.to_string()))?;
    }
    tx.commit().await?;
    Ok(record_id)
}
