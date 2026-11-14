//! 人工退款：登记、双人审核与重试（旧版 refund_review_repo.go + refund.go 的
//! 资金核心）。权限校验与审计由 HTTP 层负责，本模块只做资金语义：
//! 锁序 支付 → 退款；审核通过后转 automatic 由退款执行器接手。
//! 已结束充电必须先有费用回执，不能抢在计费之前退款。

use chargepilot_domain::money::{
    BizType, ExecutionPolicy, PayMethod, PaymentOrderStatus, RefundStatus,
};
use chargepilot_domain::order::ChargeOrderStatus;
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

use crate::AuditView;

/// 退款列表行（旧版 GORM map 投影定形：refund_record 全列 + refund_review 右表列；
/// 有审核记录时 created_at 取审核记录创建时间，与旧版右表覆盖行为一致）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RefundAdminRow {
    pub id: u64,
    pub refund_no: String,
    pub payment_order_id: u64,
    pub user_id: u64,
    pub biz_type: String,
    pub biz_id: u64,
    pub refund_cents: i64,
    pub reason: Option<String>,
    pub status: String,
    pub retry_count: u32,
    pub wechat_refund_id: Option<String>,
    pub claimed_by: Option<u64>,
    pub claimed_at: Option<String>,
    pub completed_at: Option<String>,
    pub failure_reason: Option<String>,
    pub created_month: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub deleted_by: Option<u64>,
    pub execution_policy: String,
    pub next_attempt_at: String,
    /// 以下为 refund_review 列（无审核记录时全为 null）。
    pub refund_record_id: Option<u64>,
    /// 首审快照原文（旧版输出为 JSON 文本字符串，不内嵌对象）。
    pub snapshot_json: Option<String>,
    pub first_signer: Option<u64>,
    pub first_comment: Option<String>,
    pub second_signer: Option<u64>,
    pub second_comment: Option<String>,
    pub approved_at: Option<String>,
}

/// 分页倒序读取未删除退款及最近人工复核（旧版 ListRefunds）。
/// status / refund_no 为空表示不筛选；status 由调用方按白名单校验。
pub async fn list_refunds(
    db: &MySqlPool,
    status: &str,
    refund_no: &str,
    page: u32,
    size: u32,
) -> Result<(Vec<RefundAdminRow>, i64), anyhow::Error> {
    let mut where_clauses: Vec<String> = vec!["r.`deleted_at` IS NULL".to_string()];
    let mut binds: Vec<String> = Vec::new();
    if !status.is_empty() {
        where_clauses.push("CAST(r.`status` AS CHAR) = ?".to_string());
        binds.push(status.to_string());
    }
    if !refund_no.is_empty() {
        where_clauses.push("CAST(r.`refund_no` AS CHAR) = ?".to_string());
        binds.push(refund_no.to_string());
    }
    let where_sql = where_clauses.join(" AND ");
    let count_sql = format!(
        "SELECT COUNT(*) FROM `refund_record` r \
         LEFT JOIN `refund_review` v ON v.`refund_record_id` = r.`id` WHERE {where_sql}"
    );
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        count_query = count_query.bind(b);
    }
    let total = count_query.fetch_one(db).await?;
    let offset = (page.max(1) as i64 - 1) * size.max(1) as i64;
    let list_sql = format!(
        "SELECT r.`id`, CAST(r.`refund_no` AS CHAR) AS `refund_no`, \
                r.`payment_order_id`, r.`user_id`, \
                CAST(r.`biz_type` AS CHAR) AS `biz_type`, r.`biz_id`, \
                r.`refund_cents`, CAST(r.`reason` AS CHAR) AS `reason`, \
                CAST(r.`status` AS CHAR) AS `status`, r.`retry_count`, \
                CAST(r.`wechat_refund_id` AS CHAR) AS `wechat_refund_id`, \
                r.`claimed_by`, \
                DATE_FORMAT(r.`claimed_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `claimed_at`, \
                DATE_FORMAT(r.`completed_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `completed_at`, \
                CAST(r.`failure_reason` AS CHAR) AS `failure_reason`, \
                DATE_FORMAT(r.`created_month`, '%Y-%m-%dT00:00:00+00:00') AS `created_month`, \
                DATE_FORMAT(COALESCE(v.`created_at`, r.`created_at`), '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                DATE_FORMAT(r.`updated_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `updated_at`, \
                DATE_FORMAT(r.`deleted_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `deleted_at`, \
                r.`deleted_by`, \
                CAST(r.`execution_policy` AS CHAR) AS `execution_policy`, \
                DATE_FORMAT(r.`next_attempt_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `next_attempt_at`, \
                v.`refund_record_id`, \
                CAST(v.`snapshot_json` AS CHAR) AS `snapshot_json`, \
                v.`first_signer`, CAST(v.`first_comment` AS CHAR) AS `first_comment`, \
                v.`second_signer`, CAST(v.`second_comment` AS CHAR) AS `second_comment`, \
                DATE_FORMAT(v.`approved_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `approved_at` \
         FROM `refund_record` r \
         LEFT JOIN `refund_review` v ON v.`refund_record_id` = r.`id` \
         WHERE {where_sql} ORDER BY r.`id` DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query_as::<_, RefundAdminRow>(&list_sql);
    for b in &binds {
        list_query = list_query.bind(b);
    }
    list_query = list_query.bind(size.max(1)).bind(offset);
    let rows = list_query.fetch_all(db).await?;
    Ok((rows, total))
}

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
/// 首次登记时在事务内写审计（重放不写，旧版 created 分支语义）。
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 订单 + 支付锁（支付 → 退款锁序）。
    let order: Option<(u64, u64, ChargeOrderStatus, i64)> = sqlx::query_as(
        "SELECT `payment_order_id`, `user_id`, `status`, `started_at` IS NOT NULL \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((payment_order_id, order_user_id, order_status, has_started)) = order else {
        return Err(RefundAdminError::NotFound);
    };
    let payment: Option<(u64, i64, i64, PaymentOrderStatus, PayMethod)> = sqlx::query_as(
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
    .bind(RefundStatus::Pending)
    .bind(RefundStatus::Processing)
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
    .bind(BizType::Charge)
    .bind(order_id as i64)
    .bind(amount_cents)
    .bind(reason)
    .bind(RefundStatus::Pending)
    .bind(ExecutionPolicy::ManualReview)
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
    let record_id: u64 = sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await?;
    // 首次登记审计（旧版 AuditChange{Action:"request", Target:"refund", After:in, RequestID}）。
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
/// 决定落库时在事务内写审计（驳回与双审通过都写，旧版 service 层语义）。
#[allow(clippy::too_many_arguments)]
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    let record: Option<(u64, u64, u64, i64, BizType, u64)> = sqlx::query_as(
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
    let payment: Option<(u64, i64, i64, PaymentOrderStatus, PayMethod)> = sqlx::query_as(
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
    let state: Option<(RefundStatus, ExecutionPolicy)> = sqlx::query_as(
        "SELECT `status`, `execution_policy` FROM `refund_record` WHERE `id` = ? FOR UPDATE",
    )
    .bind(record_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((status, policy)) = state else {
        return Err(RefundAdminError::NotFound);
    };
    if status != RefundStatus::Pending || policy != ExecutionPolicy::ManualReview {
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
            .bind(RefundStatus::Rejected)
            .bind(reason)
            .bind(record_id as i64)
            .execute(&mut *c)
            .await?;
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
    if biz_type == BizType::Charge {
        let (order_started, order_status): (i64, ChargeOrderStatus) = sqlx::query_as(
            "SELECT `started_at` IS NOT NULL, `status` FROM `charge_order` WHERE `id` = ?",
        )
        .bind(biz_id as i64)
        .fetch_one(&mut *c)
        .await
        .map_err(|_| RefundAdminError::Conflict)?;
        if order_started != 0
            && matches!(
                order_status,
                ChargeOrderStatus::Completed
                    | ChargeOrderStatus::Refunding
                    | ChargeOrderStatus::Refunded
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
                            .any(|p| p == "order.refund.review")
                }
            };
            if !permitted {
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
            .bind(ExecutionPolicy::Automatic)
            .bind(record_id as i64)
            .execute(&mut *c)
            .await?;
            ReviewOutcome::Approved
        }
    };
    // 决定审计（旧版 Action=string(status)，驳回 rejected、首审 awaiting_second、双审 approved）。
    let action = match outcome {
        ReviewOutcome::Rejected => "rejected",
        ReviewOutcome::AwaitingSecond => "awaiting_second",
        ReviewOutcome::Approved => "approved",
    };
    write_refund_audit(&mut tx, audit, action, record_id, approve_comment, reason).await?;
    tx.commit().await?;
    Ok(outcome)
}

/// 退款决定的审计写入（旧版 AuditChange{Target:"refund", After:in}）。
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
/// 人工待审或终态返回冲突；金额与状态不变（旧版 Retry）。重复重试静默成功并写审计。
pub async fn retry_refund(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    refund_no: &str,
    reason: &str,
) -> Result<u64, RefundAdminError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let record: Option<(u64, ExecutionPolicy, RefundStatus)> = sqlx::query_as(
        "SELECT `id`, `execution_policy`, `status` FROM `refund_record` \
         WHERE CAST(`refund_no` AS CHAR) = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(refund_no)
    .fetch_optional(&mut *c)
    .await?;
    let Some((record_id, policy, status)) = record else {
        return Err(RefundAdminError::NotFound);
    };
    let unsettled = matches!(status, RefundStatus::Pending | RefundStatus::Processing);
    if policy != ExecutionPolicy::Automatic || !unsettled {
        return Err(RefundAdminError::Conflict);
    }
    sqlx::query("UPDATE `refund_record` SET `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(record_id as i64)
        .execute(&mut *c)
        .await?;
    // 重试审计（旧版 AuditChange{Action:"retry", Target:"refund", After:in}）。
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
