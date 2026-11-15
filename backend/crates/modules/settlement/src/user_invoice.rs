//! 用户端发票申请与我的发票（旧版 settlement/invoice.go + user_invoice_repo.go
//! 的单体对应物）。开票额来自费用回执，客户端只提交票种及抬头等内容，不决定
//! 金额；一单只允许一份申请（uk_invoice_active），同号同内容重放幂等复用。
//! 可开票状态与计费任务共用同一判定：completed / refunding / refunded。

use crate::error::UserInvoiceError;
use chargepilot_domain::money::BizType;
use chargepilot_domain::settlement::{InvoiceRequestReviewStatus, InvoiceType};
use serde_json::json;
use sqlx::{Acquire, MySqlPool};

/// 申请结果：固定 pending，金额为回执确定总额。
#[derive(Debug, Clone, serde::Serialize)]
pub struct InvoiceApplication {
    /// 首次请求号派生的发票编号。
    pub invoice_no: String,
    /// 发票申请主键。
    pub id: u64,
    /// 固定 pending（旧版 InvoiceRequestPending）。
    pub review_status: &'static str,
    /// 已确认费用回执的确定总额，分。
    pub total_cents: i64,
}

/// 发票申请内容（旧版 settlement.InvoiceInput；金额由回执提供）。
#[derive(Debug, Clone, Default)]
pub struct InvoiceInput {
    pub request_id: String,
    pub order_no: String,
    pub invoice_type: String,
    pub title: String,
    pub tax_no: String,
    pub email: String,
}

/// 订单已可开票的状态判定（旧版 invoiceableStatus：发票申请与计费任务共用）。
fn invoiceable_status(status: &str) -> bool {
    matches!(status, "completed" | "refunding" | "refunded")
}

/// 合法紧凑 UUID（旧版 uuid.Validate：8-4-4-4-12 位十六进制）。
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, ch) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *ch != b'-' {
                    return false;
                }
            }
            _ => {
                if !ch.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// 核对本人订单和结算回执后申请发票。校验在服务层完整重做（旧版 Create
/// 同样全量校验 ErrInvoiceInput），HTTP 层的前置校验只负责提前给出更细文案。
pub async fn apply_invoice(
    pool: &MySqlPool,
    user_id: u64,
    input: &InvoiceInput,
) -> Result<InvoiceApplication, UserInvoiceError> {
    let title = input.title.trim();
    // 入参词表值一次 parse 存档（校验保证合法）；重放比对与 INSERT 复用。
    let invoice_type_value = InvoiceType::parse(input.invoice_type.as_str())
        .map_err(|_| UserInvoiceError::InvalidInput)?;
    if user_id == 0
        || !is_uuid(&input.request_id)
        || input.order_no.trim().is_empty()
        || InvoiceType::parse(input.invoice_type.as_str()).is_err()
        || title.is_empty()
        || title.chars().count() > 128
        || input.tax_no.trim().chars().count() > 32
        || input.email.len() > 128
        || (!input.email.is_empty() && !input.email.contains('@'))
    {
        return Err(UserInvoiceError::InvalidInput);
    }
    // 本人订单：订单号 + 用户双条件（旧版 FindInvoiceableOrder）。
    let order: Option<(u64, String)> = sqlx::query_as(
        "SELECT `id`, CAST(`status` AS CHAR) FROM `charge_order` \
         WHERE `order_no` = ? AND `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(&input.order_no)
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await
    .map_err(|e| UserInvoiceError::OrderLookup(e.to_string()))?;
    let Some((order_id, status)) = order else {
        return Err(UserInvoiceError::OrderNotFound);
    };
    if !invoiceable_status(&status) {
        return Err(UserInvoiceError::Unfinished);
    }
    // 费用回执：金额不由客户端决定（旧版 LoadInvoiceFee + Fees）。
    let total: Option<(i64,)> = sqlx::query_as(
        "SELECT CAST(JSON_UNQUOTE(JSON_EXTRACT(`result_json`, '$.total_cents')) AS SIGNED) \
         FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await
    .map_err(|e| UserInvoiceError::Storage(e.to_string()))?;
    let Some((total,)) = total else {
        return Err(UserInvoiceError::Unbilled);
    };
    if total <= 0 {
        return Err(UserInvoiceError::ZeroFee);
    }
    let invoice_no = format!("INV{}", input.request_id.replace('-', "").to_uppercase());

    // 项目事务惯例：先 SET 隔离级别再 begin（事务中 SET 会报错）。
    let mut conn = pool.acquire().await.map_err(storage_err)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    let mut tx = conn.begin().await.map_err(storage_err)?;
    let c = tx.acquire().await.map_err(storage_err)?;
    // 锁定本人订单并复查状态（旧版 LockInvoiceSourceInTx：防同单不同请求号
    // 并发重复申请；归属不符按读取故障处理，正常请求不会触发）。
    let locked: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`status` AS CHAR) FROM `charge_order` \
         WHERE `id` = ? AND `user_id` = ? FOR UPDATE",
    )
    .bind(order_id as i64)
    .bind(user_id as i64)
    .fetch_optional(&mut *c)
    .await
    .map_err(storage_err)?;
    let Some((locked_status,)) = locked else {
        return Err(UserInvoiceError::OrderLookup("订单锁定读取失败".into()));
    };
    if !invoiceable_status(&locked_status) {
        return Err(UserInvoiceError::Unfinished);
    }
    // 幂等重放：同发票号必须复用相同身份和内容（旧版 ApplyInvoice 前半）。
    // 发票幂等投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type InvoiceExistingRow = (
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
    let existing: Option<InvoiceExistingRow> = sqlx::query_as(
        "SELECT `id`, CAST(`user_id` AS CHAR), `biz_type`, \
             CAST(`biz_id` AS CHAR), `total_cents`, CAST(`invoice_type` AS CHAR), \
             CAST(`title` AS CHAR), COALESCE(CAST(`tax_no` AS CHAR), ''), \
             COALESCE(CAST(`email` AS CHAR), '') \
             FROM `invoice_request` WHERE `invoice_no` = ?",
    )
    .bind(&invoice_no)
    .fetch_optional(&mut *c)
    .await
    .map_err(storage_err)?;
    let id = match existing {
        Some((
            id,
            owner,
            biz_type,
            biz_id,
            existing_total,
            invoice_type,
            existing_title,
            tax_no,
            email,
        )) => {
            let same = owner == user_id.to_string()
                && biz_type == BizType::Charge
                && biz_id == order_id.to_string()
                && existing_total == total
                && invoice_type == invoice_type_value
                && existing_title == title
                && tax_no == input.tax_no.trim()
                && email == input.email.trim();
            if !same {
                return Err(UserInvoiceError::Duplicate);
            }
            id
        }
        None => {
            // 一单一申请：已有未删除申请即冲突（旧版 Count 分支）。
            let duplicate: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM `invoice_request` \
                 WHERE `biz_type` = ? AND `biz_id` = ? AND `deleted_at` IS NULL",
            )
            .bind(BizType::Charge)
            .bind(order_id as i64)
            .fetch_one(&mut *c)
            .await
            .map_err(storage_err)?;
            if duplicate > 0 {
                return Err(UserInvoiceError::Duplicate);
            }
            let result = sqlx::query(
                "INSERT INTO `invoice_request` \
                 (`invoice_no`, `user_id`, `biz_type`, `biz_id`, `total_cents`, \
                  `invoice_type`, `title`, `tax_no`, `email`, `review_status`) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&invoice_no)
            .bind(user_id as i64)
            .bind(BizType::Charge)
            .bind(order_id as i64)
            .bind(total)
            .bind(invoice_type_value)
            .bind(title)
            .bind(input.tax_no.trim())
            .bind(input.email.trim())
            .bind(InvoiceRequestReviewStatus::Pending)
            .execute(&mut *c)
            .await;
            // 并发撞 uk_invoice_active（biz_type, biz_id）与计数兜底同义。
            match result {
                Ok(_) => {
                    let id: u64 = sqlx::query_scalar("SELECT LAST_INSERT_ID()")
                        .fetch_one(&mut *c)
                        .await
                        .map_err(storage_err)?;
                    id
                }
                Err(e) => return Err(duplicate_or_storage(e)),
            }
        }
    };
    tx.commit().await.map_err(storage_err)?;
    Ok(InvoiceApplication {
        invoice_no,
        id,
        review_status: "pending",
        total_cents: total,
    })
}

/// 分页倒序读取本人未删除的发票申请（旧版 ListUserInvoiceRequests；
/// 字段与旧版 map 投影一致，id/biz_id 为数字，可空列输出 null）。
pub async fn list_user_invoices(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> anyhow::Result<(Vec<serde_json::Value>, i64)> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `invoice_request` WHERE `user_id` = ? AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_one(pool)
    .await?;
    // 发票列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type InvoiceListRow = (
        String,         // CAST(id AS CHAR)
        String,         // invoice_no
        String,         // biz_type
        String,         // biz_id CHAR
        i64,            // total_cents
        String,         // invoice_type
        String,         // title
        Option<String>, // tax_no
        Option<String>, // email
        String,         // review_status
        Option<String>, // reject_reason
        Option<String>, // invoice_url
        String,         // created_at
    );
    let rows: Vec<InvoiceListRow> = sqlx::query_as(
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
    .bind((page.max(1) as i64 - 1) * page_size as i64)
    .bind(page_size as i64)
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(
            |(
                id,
                invoice_no,
                biz_type,
                biz_id,
                total_cents,
                invoice_type,
                title,
                tax_no,
                email,
                review_status,
                reject_reason,
                invoice_url,
                created_at,
            )| {
                json!({
                    "id": id.parse::<u64>().unwrap_or(0),
                    "invoice_no": invoice_no,
                    "biz_type": biz_type,
                    "biz_id": biz_id.parse::<u64>().unwrap_or(0),
                    "total_cents": total_cents,
                    "invoice_type": invoice_type,
                    "title": title,
                    "tax_no": tax_no,
                    "email": email,
                    "review_status": review_status,
                    "reject_reason": reject_reason,
                    "invoice_url": invoice_url,
                    "created_at": created_at,
                })
            },
        )
        .collect();
    Ok((items, total))
}

fn storage_err(e: sqlx::Error) -> UserInvoiceError {
    UserInvoiceError::Storage(e.to_string())
}

/// 插入冲突的归一：uk_invoice_active 唯一键冲突映射重复申请，其余为存储错误。
fn duplicate_or_storage(e: sqlx::Error) -> UserInvoiceError {
    if matches!(&e, sqlx::Error::Database(db) if db.is_unique_violation()) {
        UserInvoiceError::Duplicate
    } else {
        UserInvoiceError::Storage(e.to_string())
    }
}
