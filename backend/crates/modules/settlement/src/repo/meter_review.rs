//! 计量核实与人工金额的表访问：charge_meter_review / manual_fee_review /
//! charge_manual_settlement / charge_billing_job（蓝图第五章 repo 层）。

use chargepilot_domain::settlement::{BillingJobStatus, MeterReviewStatus};
use sqlx::{MySqlConnection, MySqlPool};

/// 核实记录行的统一投影列（json 列 CAST AS CHAR，时间格式 RFC3339）。
pub const METER_REVIEW_COLUMNS: &str = "`id`, CAST(`request_id` AS CHAR) AS `request_id`, \
     `charge_order_id`, CAST(`original_json` AS CHAR) AS `original_json`, \
     CAST(`corrected_json` AS CHAR) AS `corrected_json`, \
     CAST(`reason` AS CHAR) AS `reason`, CAST(`status` AS CHAR) AS `status`, \
     `first_reviewer_id`, `second_reviewer_id`, \
     CAST(`reject_reason` AS CHAR) AS `reject_reason`, \
     DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
     DATE_FORMAT(`reviewed_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `reviewed_at`";

/// 核实记录原始行（original/corrected 为 json 文本，转出时解析为对象）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MeterReviewRawRow {
    pub id: u64,
    pub request_id: String,
    pub charge_order_id: u64,
    pub original_json: String,
    pub corrected_json: String,
    pub reason: String,
    pub status: MeterReviewStatus,
    pub first_reviewer_id: u64,
    pub second_reviewer_id: Option<u64>,
    pub reject_reason: Option<String>,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

impl MeterReviewRawRow {
    /// 转成输出行；json 文本解析失败按 null 处理，不阻断整页。
    pub fn into_row(self) -> crate::service::meter_review::MeterReviewRow {
        crate::service::meter_review::MeterReviewRow {
            id: self.id,
            request_id: self.request_id,
            charge_order_id: self.charge_order_id,
            original: serde_json::from_str(&self.original_json).unwrap_or(serde_json::Value::Null),
            corrected: serde_json::from_str(&self.corrected_json)
                .unwrap_or(serde_json::Value::Null),
            reason: self.reason,
            status: self.status,
            first_reviewer_id: self.first_reviewer_id,
            second_reviewer_id: self.second_reviewer_id,
            reject_reason: self.reject_reason,
            created_at: self.created_at,
            reviewed_at: self.reviewed_at,
        }
    }
}

/// 人工定价工单原始行抓取（json 列以字符串读出后解析，避免列类型映射歧义）。
#[derive(Debug, sqlx::FromRow)]
pub struct ManualFeeReviewRawRow {
    pub charge_order_id: u64,
    pub order_no: String,
    pub reason: String,
    pub status: String,
    pub source_json: Option<String>,
    pub created_at: String,
}

/// 读取单条核实记录。
pub async fn get_meter_review_row(
    db: &MySqlPool,
    id: u64,
) -> Result<Option<MeterReviewRawRow>, sqlx::Error> {
    let sql = format!("SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` WHERE `id` = ?");
    sqlx::query_as(&sql)
        .bind(id as i64)
        .fetch_optional(db)
        .await
}

/// 组装工单过滤子句与绑定值（status / keyword 空串表示不筛选）。
pub fn manual_review_filters(status: &str, keyword: &str) -> (String, Vec<String>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if !status.is_empty() {
        where_clauses.push("`status` = ?".to_string());
        binds.push(status.to_string());
    }
    if !keyword.is_empty() {
        where_clauses.push("`order_no` LIKE ? ESCAPE '!'".to_string());
        binds.push(like_pattern(keyword));
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };
    (where_sql, binds)
}

/// 关键词 LIKE 转义。
fn like_pattern(value: &str) -> String {
    format!(
        "%{}%",
        value
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    )
}

/// 人工定价工单计数（与列表同过滤）。
pub async fn manual_review_count(
    db: &MySqlPool,
    where_sql: &str,
    binds: &[String],
) -> Result<i64, sqlx::Error> {
    let count_sql = format!("SELECT COUNT(*) FROM `manual_fee_review` {where_sql}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in binds {
        count_query = count_query.bind(b);
    }
    count_query.fetch_one(db).await
}

/// 人工定价工单分页行（id 倒序）。
pub async fn manual_review_rows(
    db: &MySqlPool,
    where_sql: &str,
    binds: &[String],
    offset: i64,
    size: i64,
) -> Result<Vec<ManualFeeReviewRawRow>, sqlx::Error> {
    let list_sql = format!(
        "SELECT `charge_order_id`, CAST(`order_no` AS CHAR) AS `order_no`, \
                CAST(`reason` AS CHAR) AS `reason`, CAST(`status` AS CHAR) AS `status`, \
                CAST(`source_json` AS CHAR) AS `source_json`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `manual_fee_review` {where_sql} ORDER BY `id` DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query_as::<_, ManualFeeReviewRawRow>(&list_sql);
    for b in binds {
        list_query = list_query.bind(b);
    }
    list_query = list_query.bind(size).bind(offset);
    list_query.fetch_all(db).await
}

/// 按订单集合批量取核实记录（空页不查询），按订单分组、ID 倒序。
pub async fn reviews_for_orders(
    db: &MySqlPool,
    order_ids: &[u64],
) -> Result<Vec<MeterReviewRawRow>, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new(format!(
        "SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` \
         WHERE `charge_order_id` IN ("
    ));
    let mut separated = qb.separated(", ");
    for oid in order_ids {
        separated.push_bind(*oid as i64);
    }
    qb.push(") ORDER BY `id` DESC");
    qb.build_query_as().fetch_all(db).await
}

/// 锁读计费任务状态（顺序化人工出口）。
pub async fn lock_job_status_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<BillingJobStatus>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 同 request_id 重放读取（id、归属订单、原因与修正 JSON）。
pub async fn find_prior_review_tx(
    c: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, u64, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `charge_order_id`, `reason`, CAST(`corrected_json` AS CHAR) AS `corrected_json` \
         FROM `charge_meter_review` WHERE CAST(`request_id` AS CHAR) = ? FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await
}

/// 活跃提案计数（awaiting_second/approved 存在即拒绝新提案）。
pub async fn active_review_count_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_meter_review` \
         WHERE `charge_order_id` = ? AND `status` IN (?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(MeterReviewStatus::AwaitingSecond)
    .bind(MeterReviewStatus::Approved)
    .fetch_one(&mut *c)
    .await
}

/// 登记修正提案（awaiting_second，首审人随行）。
pub async fn insert_review_tx(
    c: &mut MySqlConnection,
    request_id: &str,
    charge_order_id: u64,
    original_json: &str,
    corrected_json: &str,
    reason: &str,
    actor_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_meter_review` (`request_id`, `charge_order_id`, `original_json`, \
         `corrected_json`, `reason`, `status`, `first_reviewer_id`) \
         VALUES (?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, ?)",
    )
    .bind(request_id)
    .bind(charge_order_id as i64)
    .bind(original_json)
    .bind(corrected_json)
    .bind(reason)
    .bind(MeterReviewStatus::AwaitingSecond)
    .bind(actor_id as i64)
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

/// 审前行快照。
pub async fn lock_review_row_tx(
    c: &mut MySqlConnection,
    review_id: u64,
    charge_order_id: u64,
) -> Result<Option<MeterReviewRawRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` \
         WHERE `id` = ? AND `charge_order_id` = ? FOR UPDATE"
    );
    sqlx::query_as(&sql)
        .bind(review_id as i64)
        .bind(charge_order_id as i64)
        .fetch_optional(&mut *c)
        .await
}

/// 批准：登记第二审核人。
pub async fn approve_review_tx(
    c: &mut MySqlConnection,
    review_id: u64,
    actor_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_meter_review` SET `status` = ?, `second_reviewer_id` = ?, \
         `reviewed_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(MeterReviewStatus::Approved)
    .bind(actor_id as i64)
    .bind(review_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 驳回：记录依据与第二审核人。
pub async fn reject_review_tx(
    c: &mut MySqlConnection,
    review_id: u64,
    actor_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_meter_review` SET `status` = ?, `second_reviewer_id` = ?, \
         `reviewed_at` = UTC_TIMESTAMP(3), `reject_reason` = ? WHERE `id` = ?",
    )
    .bind(MeterReviewStatus::Rejected)
    .bind(actor_id as i64)
    .bind(reason)
    .bind(review_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 重排队计费：修正分段参与重算（generation 递增防旧派发覆盖）。
pub async fn requeue_job_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_billing_job` SET `status` = ?, `generation` = `generation` + 1, \
         `next_attempt_at` = UTC_TIMESTAMP(3), `last_error` = NULL WHERE `charge_order_id` = ?",
    )
    .bind(BillingJobStatus::Pending)
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 人工金额重放读取（一单一金额 + 同号同内容）。
pub async fn find_prior_settlement_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    request_id: &str,
) -> Result<Option<(u64, String, i64, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `charge_order_id`, CAST(`request_id` AS CHAR) AS `request_id`, \
                `electric_cents`, `service_cents`, `reason` \
         FROM `charge_manual_settlement` \
         WHERE `charge_order_id` = ? OR `request_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await
}

/// 登记人工最终金额。
pub async fn insert_manual_settlement_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    request_id: &str,
    actor_id: u64,
    electric_cents: i64,
    service_cents: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_manual_settlement` (`charge_order_id`, `request_id`, `actor_id`, \
         `electric_cents`, `service_cents`, `reason`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(request_id)
    .bind(actor_id as i64)
    .bind(electric_cents)
    .bind(service_cents)
    .bind(reason)
    .execute(&mut *c)
    .await?;
    Ok(())
}
