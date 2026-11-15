//! 人工退款的表访问：refund_record / refund_review / refund_rejection /
//! manual_refund_request / payment_order / charge_order / charge_fee_receipt
//! （蓝图第五章 repo 层）。

use chargepilot_domain::money::{
    BizType, ExecutionPolicy, PayMethod, PaymentOrderStatus, RefundStatus,
};
use chargepilot_domain::order::ChargeOrderStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 退款列表行（旧版 GORM map 投影定形：refund_record 全列 + refund_review 右表列；
/// 有审核记录时 created_at 取审核记录创建时间，与旧版右表覆盖行为一致）。
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
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

/// 组装列表过滤子句与绑定值（status / refund_no 为空表示不筛选）。
pub fn refund_filters(status: &str, refund_no: &str) -> (String, Vec<String>) {
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
    (where_clauses.join(" AND "), binds)
}

/// 计数（与列表同过滤）。
pub async fn refund_count(
    db: &MySqlPool,
    where_sql: &str,
    binds: &[String],
) -> Result<i64, sqlx::Error> {
    let count_sql = format!(
        "SELECT COUNT(*) FROM `refund_record` r \
         LEFT JOIN `refund_review` v ON v.`refund_record_id` = r.`id` WHERE {where_sql}"
    );
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in binds {
        count_query = count_query.bind(b);
    }
    count_query.fetch_one(db).await
}

/// 分页倒序读取未删除退款及最近人工复核（旧版 ListRefunds）。
pub async fn refund_rows(
    db: &MySqlPool,
    where_sql: &str,
    binds: &[String],
    offset: i64,
    size: i64,
) -> Result<Vec<RefundAdminRow>, sqlx::Error> {
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
    for b in binds {
        list_query = list_query.bind(b);
    }
    list_query = list_query.bind(size).bind(offset);
    list_query.fetch_all(db).await
}

/// 锁订单（支付 → 退款锁序第一步；has_started 以 started_at IS NOT NULL 表达）。
pub async fn lock_refund_order_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<(u64, u64, ChargeOrderStatus, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `payment_order_id`, `user_id`, `status`, `started_at` IS NOT NULL \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 支付事实投影元组（列序即 SELECT 顺序）。
pub type PayFactsRow = (u64, i64, i64, PaymentOrderStatus, PayMethod);

/// 锁支付单读资金与渠道事实。
pub async fn lock_pay_facts_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<Option<PayFactsRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `paid_cents`, `refunded_cents`, `status`, `pay_method` \
         FROM `payment_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 锁读人工退款请求记录（重放比对首次快照；支付行锁不刷新旧快照）。
pub async fn lock_manual_request_tx(
    c: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`payload_json` AS CHAR) AS `payload_json`, CAST(`refund_no` AS CHAR) AS `refund_no` \
         FROM `manual_refund_request` WHERE CAST(`request_id` AS CHAR) = ? FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await
}

/// 已结束充电的费用回执存在性（先入账才能退款）。
pub async fn fee_receipt_exists_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<bool, sqlx::Error> {
    let receipt: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM `charge_fee_receipt` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_optional(&mut *c)
            .await?;
    Ok(receipt.is_some())
}

/// 待退聚合（当前读，纳入锁等待期间提交的预占）。
pub async fn pending_refund_sum_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(`refund_cents`), 0) AS SIGNED) FROM `refund_record` \
         WHERE `payment_order_id` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .bind(RefundStatus::Pending)
    .bind(RefundStatus::Processing)
    .fetch_one(&mut *c)
    .await
}

/// 登记人工退款（pending + manual_review，立即排队供审核后转接）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_manual_refund_tx(
    c: &mut MySqlConnection,
    refund_no: &str,
    payment_order_id: u64,
    user_id: u64,
    order_id: u64,
    amount_cents: i64,
    reason: &str,
    month: chrono::NaiveDate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `next_attempt_at`, \
         `created_month`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3), ?)",
    )
    .bind(refund_no)
    .bind(payment_order_id as i64)
    .bind(user_id as i64)
    .bind(BizType::Charge)
    .bind(order_id as i64)
    .bind(amount_cents)
    .bind(reason)
    .bind(RefundStatus::Pending)
    .bind(ExecutionPolicy::ManualReview)
    .bind(month)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 保存人工退款请求存档（幂等重放依据）。
pub async fn insert_manual_request_tx(
    c: &mut MySqlConnection,
    request_id: &str,
    payload_json: &str,
    refund_no: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `manual_refund_request` (`request_id`, `payload_json`, `refund_no`) \
         VALUES (?, ?, ?)",
    )
    .bind(request_id)
    .bind(payload_json)
    .bind(refund_no)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 当前连接的 LAST_INSERT_ID。
pub async fn last_insert_id_tx(c: &mut MySqlConnection) -> Result<u64, sqlx::Error> {
    sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await
}

/// 退款记录本体（审核入口读取）。
pub async fn find_refund_record_tx(
    c: &mut MySqlConnection,
    refund_no: &str,
) -> Result<Option<(u64, u64, u64, i64, BizType, u64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `payment_order_id`, `user_id`, `refund_cents`, `biz_type`, `biz_id` \
         FROM `refund_record` WHERE CAST(`refund_no` AS CHAR) = ? AND `deleted_at` IS NULL",
    )
    .bind(refund_no)
    .fetch_optional(&mut *c)
    .await
}

/// 锁读退款状态（仅 pending + manual_review 可审，判定在调用方）。
pub async fn lock_refund_state_tx(
    c: &mut MySqlConnection,
    record_id: u64,
) -> Result<Option<(RefundStatus, ExecutionPolicy)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `status`, `execution_policy` FROM `refund_record` WHERE `id` = ? FOR UPDATE",
    )
    .bind(record_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 驳回：写拒绝记录。
pub async fn insert_rejection_tx(
    c: &mut MySqlConnection,
    record_id: u64,
    actor_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_rejection` (`refund_record_id`, `actor_id`, `reason`) \
         VALUES (?, ?, ?)",
    )
    .bind(record_id as i64)
    .bind(actor_id as i64)
    .bind(reason)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 驳回落状态（rejected + 失败原因）。
pub async fn mark_refund_rejected_tx(
    c: &mut MySqlConnection,
    record_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `refund_record` SET `status` = ?, `failure_reason` = ? WHERE `id` = ?")
        .bind(RefundStatus::Rejected)
        .bind(reason)
        .bind(record_id as i64)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 订单启动与状态（审核准入第二段）。
pub async fn order_started_status_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<(i64, ChargeOrderStatus), sqlx::Error> {
    sqlx::query_as("SELECT `started_at` IS NOT NULL, `status` FROM `charge_order` WHERE `id` = ?")
        .bind(order_id as i64)
        .fetch_one(&mut *c)
        .await
}

/// 锁读审核记录（快照、首审人与二审人）。
pub async fn lock_review_tx(
    c: &mut MySqlConnection,
    record_id: u64,
) -> Result<Option<(String, u64, Option<u64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`snapshot_json` AS CHAR) AS `snapshot_json`, `first_signer`, `second_signer` \
         FROM `refund_review` WHERE `refund_record_id` = ? FOR UPDATE",
    )
    .bind(record_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 首审：保存快照与首审人，等待第二审。
pub async fn insert_first_review_tx(
    c: &mut MySqlConnection,
    record_id: u64,
    snapshot: &str,
    actor_id: u64,
    approve_comment: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_review` (`refund_record_id`, `snapshot_json`, \
         `first_signer`, `first_comment`) VALUES (?, ?, ?, ?)",
    )
    .bind(record_id as i64)
    .bind(snapshot)
    .bind(actor_id as i64)
    .bind(approve_comment)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 二审：登记第二审核人。
pub async fn approve_second_review_tx(
    c: &mut MySqlConnection,
    record_id: u64,
    actor_id: u64,
    approve_comment: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_review` SET `second_signer` = ?, `second_comment` = ?, \
         `approved_at` = UTC_TIMESTAMP(3) WHERE `refund_record_id` = ?",
    )
    .bind(actor_id as i64)
    .bind(approve_comment)
    .bind(record_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 双审通过后转 automatic 并立即排队（退款执行器接手渠道执行）。
pub async fn promote_to_automatic_tx(
    c: &mut MySqlConnection,
    record_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `refund_record` SET `execution_policy` = ?, \
         `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(ExecutionPolicy::Automatic)
    .bind(record_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 锁读退款记录（重试入口）。
pub async fn lock_retry_record_tx(
    c: &mut MySqlConnection,
    refund_no: &str,
) -> Result<Option<(u64, ExecutionPolicy, RefundStatus)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `execution_policy`, `status` FROM `refund_record` \
         WHERE CAST(`refund_no` AS CHAR) = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(refund_no)
    .fetch_optional(&mut *c)
    .await
}

/// 重置排队时刻（立即重试）。
pub async fn reset_next_attempt_tx(
    c: &mut MySqlConnection,
    record_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `refund_record` SET `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(record_id as i64)
        .execute(&mut *c)
        .await?;
    Ok(())
}
