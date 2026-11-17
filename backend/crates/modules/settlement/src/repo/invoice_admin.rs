//! 发票后台队列与双审的表访问：invoice_request / invoice_admin_review（蓝图第五章 repo 层）。

use chargepilot_domain::settlement::{InvoiceAdminReviewStatus, InvoiceRequestReviewStatus};
use sqlx::{MySqlConnection, MySqlPool};

/// 后台发票队列行。
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct InvoiceAdminRow {
    pub invoice_request_id: u64,
    pub invoice_no: String,
    /// 用户 ID 转字符串。
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
    /// 审核队形的当前状态：优先取审核表，无记录回退申请单。
    pub queue_review_status: String,
    pub queue_reject_reason: Option<String>,
    pub first_reviewer_id: Option<String>,
    pub second_reviewer_id: Option<String>,
    /// 申请单上登记的拒绝原因。
    pub reject_reason: Option<String>,
    /// 发票链接：优先取审核表（二审登记的生效链接）。
    pub invoice_url: Option<String>,
    pub created_at: String,
}

/// 全量倒序读取未删除发票申请及人工队列结论。
/// queue_ 前缀字段是审核进度汇总：优先取审核表，没有再回退申请单本身。
pub async fn list_invoice_requests(db: &MySqlPool) -> Result<Vec<InvoiceAdminRow>, sqlx::Error> {
    sqlx::query_as::<_, InvoiceAdminRow>(
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
    .await
}

/// 锁读申请单本体：审核状态与已有发票链接（审计 before 快照同源）。
pub async fn lock_invoice_request_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<(InvoiceRequestReviewStatus, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `review_status`, `invoice_url` \
         FROM `invoice_request` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 双审进度投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type InvoiceReviewRow = (
    InvoiceAdminReviewStatus,
    Option<u64>,
    Option<u64>,
    Option<String>,
);

/// 双审进度表读取：awaiting_second 表示已首审待复核；无记录表示尚未有任何审核。
pub async fn find_admin_review_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<InvoiceReviewRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `review_status`, `first_reviewer_id`, `second_reviewer_id`, `invoice_url` \
         FROM `invoice_admin_review` WHERE `invoice_request_id` = ?",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 单人驳回：登记/覆写驳回结论并清空发票链接。
pub async fn reject_admin_review_tx(
    c: &mut MySqlConnection,
    id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
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
    Ok(())
}

/// 申请单置驳回（记录审核人与原因）。
pub async fn mark_request_rejected_tx(
    c: &mut MySqlConnection,
    id: u64,
    actor_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `invoice_request` SET `review_status` = ?, `reviewed_by` = ?, \
         `reviewed_at` = UTC_TIMESTAMP(3), `reject_reason` = ? WHERE `id` = ?",
    )
    .bind(InvoiceRequestReviewStatus::Rejected)
    .bind(actor_id as i64)
    .bind(reason)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 首审：登记首审人与发票链接，等待第二人复核。
pub async fn insert_first_review_tx(
    c: &mut MySqlConnection,
    id: u64,
    actor_id: u64,
    invoice_url: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `invoice_admin_review` \
         (`invoice_request_id`, `review_status`, `first_reviewer_id`, `invoice_url`) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(id as i64)
    .bind(InvoiceAdminReviewStatus::AwaitingSecond)
    .bind(actor_id as i64)
    .bind(invoice_url)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 复核通过：登记第二审核人。
pub async fn approve_admin_review_tx(
    c: &mut MySqlConnection,
    id: u64,
    actor_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `invoice_admin_review` SET `review_status` = ?, \
         `second_reviewer_id` = ? WHERE `invoice_request_id` = ?",
    )
    .bind(InvoiceAdminReviewStatus::Approved)
    .bind(actor_id as i64)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 申请单置已开票（生效链接与二审人落申请单）。
pub async fn mark_request_issued_tx(
    c: &mut MySqlConnection,
    id: u64,
    invoice_url: &str,
    actor_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `invoice_request` SET `review_status` = ?, `invoice_url` = ?, \
         `reviewed_by` = ?, `reviewed_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(InvoiceRequestReviewStatus::Issued)
    .bind(invoice_url)
    .bind(actor_id as i64)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}
