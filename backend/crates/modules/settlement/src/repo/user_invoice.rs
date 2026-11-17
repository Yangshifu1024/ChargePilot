//! 用户端发票申请与我的发票的表访问：invoice_request / charge_fee_receipt /
//! charge_order（蓝图第五章 repo 层）。

use chargepilot_domain::money::BizType;
use chargepilot_domain::settlement::{InvoiceRequestReviewStatus, InvoiceType};
use sqlx::{MySqlConnection, MySqlPool};

/// 本人订单读取（订单号 + 用户双条件，FindInvoiceableOrder）。
pub async fn find_user_order(
    pool: &MySqlPool,
    order_no: &str,
    user_id: u64,
) -> Result<Option<(u64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, CAST(`status` AS CHAR) FROM `charge_order` \
         WHERE `order_no` = ? AND `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(order_no)
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await
}

/// 费用回执确定总额（金额不由客户端决定，LoadInvoiceFee + Fees）。
pub async fn fee_total(pool: &MySqlPool, charge_order_id: u64) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT CAST(JSON_UNQUOTE(JSON_EXTRACT(`result_json`, '$.total_cents')) AS SIGNED) \
         FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(total,)| total))
}

/// 锁定本人订单并复查状态（LockInvoiceSourceInTx：防同单不同请求号
/// 并发重复申请；归属不符按读取故障处理，正常请求不会触发）。
pub async fn lock_order_status_tx(
    c: &mut MySqlConnection,
    order_id: u64,
    user_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`status` AS CHAR) FROM `charge_order` \
         WHERE `id` = ? AND `user_id` = ? FOR UPDATE",
    )
    .bind(order_id as i64)
    .bind(user_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    Ok(row.map(|(status,)| status))
}

/// 发票幂等投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type InvoiceExistingRow = (
    u64,
    String,
    BizType,
    String,
    i64,
    InvoiceType,
    String,
    String,
    String,
);

/// 幂等重放读取：同发票号必须复用相同身份和内容（比对在调用方）。
pub async fn find_existing_invoice_tx(
    c: &mut MySqlConnection,
    invoice_no: &str,
) -> Result<Option<InvoiceExistingRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, CAST(`user_id` AS CHAR), `biz_type`, \
             CAST(`biz_id` AS CHAR), `total_cents`, CAST(`invoice_type` AS CHAR), \
             CAST(`title` AS CHAR), COALESCE(CAST(`tax_no` AS CHAR), ''), \
             COALESCE(CAST(`email` AS CHAR), '') \
             FROM `invoice_request` WHERE `invoice_no` = ?",
    )
    .bind(invoice_no)
    .fetch_optional(&mut *c)
    .await
}

/// 一单一申请：已有未删除申请计数。
pub async fn active_invoice_count_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `invoice_request` \
         WHERE `biz_type` = ? AND `biz_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(BizType::Charge)
    .bind(order_id as i64)
    .fetch_one(&mut *c)
    .await
}

/// 创建发票申请（uk_invoice_active 冲突原样上抛由调用方归类）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_invoice_tx(
    c: &mut MySqlConnection,
    invoice_no: &str,
    user_id: u64,
    order_id: u64,
    total: i64,
    invoice_type: InvoiceType,
    title: &str,
    tax_no: &str,
    email: &str,
) -> Result<sqlx::mysql::MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO `invoice_request` \
         (`invoice_no`, `user_id`, `biz_type`, `biz_id`, `total_cents`, \
          `invoice_type`, `title`, `tax_no`, `email`, `review_status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(invoice_no)
    .bind(user_id as i64)
    .bind(BizType::Charge)
    .bind(order_id as i64)
    .bind(total)
    .bind(invoice_type)
    .bind(title)
    .bind(tax_no)
    .bind(email)
    .bind(InvoiceRequestReviewStatus::Pending)
    .execute(&mut *c)
    .await
}

/// 当前连接的 LAST_INSERT_ID。
pub async fn last_insert_id_tx(c: &mut MySqlConnection) -> Result<u64, sqlx::Error> {
    sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await
}

/// 本人发票申请计数。
pub async fn user_invoice_count(pool: &MySqlPool, user_id: u64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `invoice_request` WHERE `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_one(pool)
    .await
}

/// 发票列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type InvoiceListRow = (
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
);

/// 分页倒序读取本人未删除的发票申请。
pub async fn user_invoice_rows(
    pool: &MySqlPool,
    user_id: u64,
    offset: i64,
    page_size: i64,
) -> Result<Vec<InvoiceListRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`id` AS CHAR), CAST(`invoice_no` AS CHAR), CAST(`biz_type` AS CHAR), \
         CAST(`biz_id` AS CHAR), `total_cents`, CAST(`invoice_type` AS CHAR), \
         CAST(`title` AS CHAR), CAST(`tax_no` AS CHAR), CAST(`email` AS CHAR), \
         CAST(`review_status` AS CHAR), CAST(`reject_reason` AS CHAR), \
         CAST(`invoice_url` AS CHAR), \
         DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') \
         FROM `invoice_request` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL \
         ORDER BY `id` DESC LIMIT ?, ?",
    )
    .bind(user_id as i64)
    .bind(offset)
    .bind(page_size)
    .fetch_all(pool)
    .await
}
