//! 发票申请后台队列与双审（旧版 ListInvoiceRequests + InvoiceReviewRepo.Review 的单体对应物）。
//! 列表为全量倒序（旧版无分页参数），user_id 与审核人 ID 输出十进制字符串防止 JS 精度丢失。
//! 双审：通过须两名不同且仍有 invoice.review 权限的 customer_finance，两次提交的发票链接一致；
//! 驳回单人即可。审核表、申请单状态与审计同事务提交；同人复核与状态不符一律冲突。

use chargepilot_domain::settlement::{InvoiceAdminReviewStatus, InvoiceRequestReviewStatus};
use chargepilot_mod_ops::Profile;
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

/// 后台发票队列行（旧版 map 投影定形；JSON 字段名与旧版一致）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvoiceAdminRow {
    pub invoice_request_id: u64,
    pub invoice_no: String,
    /// 用户 ID 转字符串（旧版 DecimalizeRowIDs）。
    pub user_id: String,
    pub biz_type: String,
    pub biz_id: u64,
    pub total_cents: i64,
    pub invoice_type: String,
    pub title: String,
    pub tax_no: Option<String>,
    pub email: Option<String>,
    /// 申请单当前审核状态（pending/approved/rejected/issued）。
    pub review_status: String,
    /// 审核队形的当前状态：优先取审核表，无记录回退申请单（旧版 COALESCE）。
    pub queue_review_status: String,
    pub queue_reject_reason: Option<String>,
    pub first_reviewer_id: Option<String>,
    pub second_reviewer_id: Option<String>,
    /// 申请单上登记的拒绝原因（旧版 i.reject_reason）。
    pub reject_reason: Option<String>,
    /// 发票链接：优先取审核表（二审登记的生效链接）。
    pub invoice_url: Option<String>,
    pub created_at: String,
}

/// 发票审核域错误（语义对齐旧版 ErrInvoiceConflict 族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvoiceAdminError {
    /// 发票申请不存在或已删除。
    NotFound,
    /// 状态、签名或内容冲突（旧版 ErrInvoiceConflict）。
    Conflict,
    /// 服务内实时权限复核失败（旧版 ErrForbidden；正常请求已被 HTTP 层拦截）。
    Forbidden,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for InvoiceAdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvoiceAdminError::NotFound => f.write_str("发票申请不存在"),
            InvoiceAdminError::Conflict => f.write_str("发票审核状态冲突"),
            InvoiceAdminError::Forbidden => f.write_str("当前账号无权执行此操作"),
            InvoiceAdminError::Storage(m) => write!(f, "发票审核存储错误: {m}"),
        }
    }
}

impl std::error::Error for InvoiceAdminError {}

impl From<sqlx::Error> for InvoiceAdminError {
    fn from(e: sqlx::Error) -> Self {
        InvoiceAdminError::Storage(e.to_string())
    }
}

/// 全量倒序读取未删除发票申请及人工队列结论（旧版 ListInvoiceRequests）。
/// queue_ 前缀字段是审核进度汇总：优先取审核表，没有再回退申请单本身。
pub async fn list_invoice_requests(db: &MySqlPool) -> Result<Vec<InvoiceAdminRow>, anyhow::Error> {
    let rows = sqlx::query_as::<_, InvoiceAdminRow>(
        "SELECT i.`id` AS `invoice_request_id`, \
                CAST(i.`invoice_no` AS CHAR) AS `invoice_no`, \
                CAST(i.`user_id` AS CHAR) AS `user_id`, \
                CAST(i.`biz_type` AS CHAR) AS `biz_type`, \
                i.`biz_id`, i.`total_cents`, \
                CAST(i.`invoice_type` AS CHAR) AS `invoice_type`, \
                CAST(i.`title` AS CHAR) AS `title`, \
                CAST(i.`tax_no` AS CHAR) AS `tax_no`, \
                CAST(i.`email` AS CHAR) AS `email`, \
                CAST(i.`review_status` AS CHAR) AS `review_status`, \
                CAST(COALESCE(r.`review_status`, i.`review_status`) AS CHAR) AS `queue_review_status`, \
                CAST(r.`reject_reason` AS CHAR) AS `queue_reject_reason`, \
                CAST(r.`first_reviewer_id` AS CHAR) AS `first_reviewer_id`, \
                CAST(r.`second_reviewer_id` AS CHAR) AS `second_reviewer_id`, \
                CAST(i.`reject_reason` AS CHAR) AS `reject_reason`, \
                CAST(COALESCE(r.`invoice_url`, i.`invoice_url`) AS CHAR) AS `invoice_url`, \
                DATE_FORMAT(i.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `invoice_request` i \
         LEFT JOIN `invoice_admin_review` r ON r.`invoice_request_id` = i.`id` \
         WHERE i.`deleted_at` IS NULL \
         ORDER BY i.`id` DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// 执行发票通过或驳回（旧版 InvoiceReviewService.Review + InvoiceReviewRepo.Review）。
/// 审核人角色与权限在本函数内实时复核；二审通过前用事务内一致读取复查首位
/// 审核人的当前角色与权限（旧版 LookupReviewer 语义）。
/// 返回决定后的队列状态：awaiting_second / approved / rejected。
#[allow(clippy::too_many_arguments)]
pub async fn review_invoice(
    db: &MySqlPool,
    actor: &Profile,
    id: u64,
    approve: bool,
    invoice_url: &str,
    reason: &str,
    client_ip: &str,
    request_id: &str,
) -> Result<&'static str, InvoiceAdminError> {
    // 服务内实时复核（旧版 audit.Actor.Role / HasPermission 判定）。
    if actor.role != "customer_finance" || !actor.permissions.iter().any(|p| p == "invoice.review")
    {
        return Err(InvoiceAdminError::Forbidden);
    }
    let mut conn = db.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 申请单本体：只取审核状态与已有发票链接（审计 before 快照同源）。
    let invoice: Option<(InvoiceRequestReviewStatus, Option<String>)> = sqlx::query_as(
        "SELECT `review_status`, `invoice_url` \
         FROM `invoice_request` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((request_status, request_url)) = invoice else {
        return Err(InvoiceAdminError::NotFound);
    };
    let before = serde_json::json!({
        "review_status": request_status,
        "invoice_url": request_url,
    });
    if request_status != InvoiceRequestReviewStatus::Pending {
        return Err(InvoiceAdminError::Conflict);
    }
    // 双审进度表：awaiting_second 表示已首审待复核；无记录表示尚未有任何审核。
    // 投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type InvoiceReviewRow = (
        InvoiceAdminReviewStatus,
        Option<u64>,
        Option<u64>,
        Option<String>,
    );
    let review: Option<InvoiceReviewRow> = sqlx::query_as(
        "SELECT `review_status`, `first_reviewer_id`, `second_reviewer_id`, `invoice_url` \
         FROM `invoice_admin_review` WHERE `invoice_request_id` = ?",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let after = serde_json::json!({
        "invoice_url": invoice_url,
        "reason": reason,
    });
    if !approve {
        // 尚无审核记录或已首审待复核时允许单人驳回。
        if let Some((review_status, _, _, _)) = &review
            && *review_status != InvoiceAdminReviewStatus::AwaitingSecond
        {
            return Err(InvoiceAdminError::Conflict);
        }
        sqlx::query(
            "INSERT INTO `invoice_admin_review` \
             (`invoice_request_id`, `review_status`, `reject_reason`) VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE `review_status` = VALUES(`review_status`), \
             `reject_reason` = VALUES(`reject_reason`), `invoice_url` = NULL",
        )
        .bind(id as i64)
        .bind(InvoiceAdminReviewStatus::Rejected)
        .bind(reason)
        .execute(&mut *c)
        .await?;
        sqlx::query(
            "UPDATE `invoice_request` SET `review_status` = ?, `reviewed_by` = ?, \
             `reviewed_at` = UTC_TIMESTAMP(3), `reject_reason` = ? WHERE `id` = ?",
        )
        .bind(InvoiceRequestReviewStatus::Rejected)
        .bind(actor.id as i64)
        .bind(reason)
        .bind(id as i64)
        .execute(&mut *c)
        .await?;
        append_audit(
            &mut tx, actor, "rejected", id, &before, &after, client_ip, request_id,
        )
        .await?;
        tx.commit().await?;
        return Ok("rejected");
    }
    let status: &'static str = match &review {
        // 首审：登记首审人与发票链接，等待第二人复核。
        None => {
            sqlx::query(
                "INSERT INTO `invoice_admin_review` \
                 (`invoice_request_id`, `review_status`, `first_reviewer_id`, `invoice_url`) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(id as i64)
            .bind(InvoiceAdminReviewStatus::AwaitingSecond)
            .bind(actor.id as i64)
            .bind(invoice_url)
            .execute(&mut *c)
            .await?;
            "awaiting_second"
        }
        Some((review_status, first_reviewer_id, _, review_url)) => {
            let Some(first) = first_reviewer_id else {
                // 审核记录存在却无首审人：状态不完整，拒绝（旧版会走到重复主键失败）。
                return Err(InvoiceAdminError::Conflict);
            };
            if *first == actor.id
                || *review_status != InvoiceAdminReviewStatus::AwaitingSecond
                || review_url.is_none()
                || review_url.as_deref() != Some(invoice_url)
            {
                return Err(InvoiceAdminError::Conflict);
            }
            // 复查首位审核人的当前角色与权限（不信任首审时保存的身份）。
            // 旧版经注入的 LookupReviewer 查询；此处用事务内一致读取（权限 FOR SHARE）。
            let first_profile = chargepilot_mod_ops::repo::load_profile_tx(&mut *c, *first)
                .await
                .map_err(|_| InvoiceAdminError::Conflict)?;
            if first_profile.role != "customer_finance"
                || !first_profile
                    .permissions
                    .iter()
                    .any(|p| p == "invoice.review")
            {
                return Err(InvoiceAdminError::Conflict);
            }
            sqlx::query(
                "UPDATE `invoice_admin_review` SET `review_status` = ?, \
                 `second_reviewer_id` = ? WHERE `invoice_request_id` = ?",
            )
            .bind(InvoiceAdminReviewStatus::Approved)
            .bind(actor.id as i64)
            .bind(id as i64)
            .execute(&mut *c)
            .await?;
            sqlx::query(
                "UPDATE `invoice_request` SET `review_status` = ?, `invoice_url` = ?, \
                 `reviewed_by` = ?, `reviewed_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
            )
            .bind(InvoiceRequestReviewStatus::Issued)
            .bind(invoice_url)
            .bind(actor.id as i64)
            .bind(id as i64)
            .execute(&mut *c)
            .await?;
            "approved"
        }
    };
    append_audit(
        &mut tx, actor, status, id, &before, &after, client_ip, request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(status)
}

/// 审核队列的审计写入（旧版 AppendAuditInTx：module/target 同为 invoice）。
#[allow(clippy::too_many_arguments)]
async fn append_audit(
    tx: &mut sqlx::MySqlConnection,
    actor: &Profile,
    action: &str,
    id: u64,
    before: &serde_json::Value,
    after: &serde_json::Value,
    client_ip: &str,
    request_id: &str,
) -> Result<(), InvoiceAdminError> {
    chargepilot_mod_ops::repo::append_audit_tx(
        tx,
        actor,
        "invoice",
        action,
        "invoice",
        &id.to_string(),
        before,
        after,
        client_ip,
        request_id,
    )
    .await
    .map_err(|e| InvoiceAdminError::Storage(e.to_string()))
}
