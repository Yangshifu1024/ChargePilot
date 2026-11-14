//! 计量核实（仪表审核）与人工最终金额（旧版 meter_review_repo.go + manual_repo.go）。
//! 计费任务 manual_review 后的两条人工出口：
//! 1. 修正计量分段（双人审核，批准后重排队计费重算）；
//! 2. 直接登记最终金额（合计不超过套餐价格，重排队按人工金额入账）。
//!    权限与审计由 HTTP 层负责；首审人实时权限由调用方传入。

use chargepilot_domain::charging_actual::price_actual;
use chargepilot_domain::charging_actual::{ActualMeter, EndMeter, Rule};
use chargepilot_domain::settlement::{BillingJobStatus, MeterReviewStatus};
use chargepilot_mod_payment::checkout::PricingSnapshot;
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

use crate::AuditView;

/// 计量核实记录行（旧版 MeterReview JSON 定形；original/corrected 内嵌对象）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MeterReviewRow {
    pub id: u64,
    pub request_id: String,
    pub charge_order_id: u64,
    /// 修正前的价格与设备计量 JSON（对象，非转义文本）。
    pub original: serde_json::Value,
    /// 只替换分段后的价格与计量 JSON。
    pub corrected: serde_json::Value,
    pub reason: String,
    /// awaiting_second / approved / rejected（charge_meter_review.status 词表）。
    pub status: MeterReviewStatus,
    pub first_reviewer_id: u64,
    pub second_reviewer_id: Option<u64>,
    pub reject_reason: Option<String>,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

/// 人工定价工单行（旧版 ManualFeeReviewListRow 定形；reviews 由调用方回填）。
#[derive(Debug, Clone, Serialize)]
pub struct ManualFeeReviewRow {
    pub charge_order_id: u64,
    pub order_no: String,
    pub reason: String,
    /// pending 待核实、resolved 已处置。
    pub status: String,
    /// 设备上报的原始计量快照（对象）；user_id 已转字符串（旧版 DecimalizeIDField）。
    pub source: Option<serde_json::Value>,
    pub created_at: String,
    /// 该订单下的核实记录，按 ID 倒序（旧版 ListMeterReviewsByOrder）。
    pub reviews: Vec<MeterReviewRow>,
}

/// 原始行抓取（json 列以字符串读出后解析，避免列类型映射歧义）。
#[derive(Debug, sqlx::FromRow)]
struct ManualFeeReviewRawRow {
    charge_order_id: u64,
    order_no: String,
    reason: String,
    status: String,
    source_json: Option<String>,
    created_at: String,
}

/// 核实记录原始行（original/corrected 为 json 文本，转出时解析为对象）。
#[derive(Debug, Clone, sqlx::FromRow)]
struct MeterReviewRawRow {
    id: u64,
    request_id: String,
    charge_order_id: u64,
    original_json: String,
    corrected_json: String,
    reason: String,
    status: MeterReviewStatus,
    first_reviewer_id: u64,
    second_reviewer_id: Option<u64>,
    reject_reason: Option<String>,
    created_at: String,
    reviewed_at: Option<String>,
}

impl MeterReviewRawRow {
    /// 转成输出行；json 文本解析失败按 null 处理，不阻断整页。
    fn into_row(self) -> MeterReviewRow {
        MeterReviewRow {
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

/// 核实记录行的统一投影列（json 列 CAST AS CHAR，时间对齐旧版 RFC3339 形状）。
const METER_REVIEW_COLUMNS: &str = "`id`, CAST(`request_id` AS CHAR) AS `request_id`, \
     `charge_order_id`, CAST(`original_json` AS CHAR) AS `original_json`, \
     CAST(`corrected_json` AS CHAR) AS `corrected_json`, \
     CAST(`reason` AS CHAR) AS `reason`, CAST(`status` AS CHAR) AS `status`, \
     `first_reviewer_id`, `second_reviewer_id`, \
     CAST(`reject_reason` AS CHAR) AS `reject_reason`, \
     DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
     DATE_FORMAT(`reviewed_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `reviewed_at`";

/// 读取单条核实记录（旧版 propose HTTP 返回完整行的查询来源）。
pub async fn get_meter_review_row(
    db: &MySqlPool,
    id: u64,
) -> Result<Option<MeterReviewRow>, anyhow::Error> {
    let sql = format!("SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` WHERE `id` = ?");
    let row: Option<MeterReviewRawRow> = sqlx::query_as(&sql)
        .bind(id as i64)
        .fetch_optional(db)
        .await?;
    Ok(row.map(MeterReviewRawRow::into_row))
}

/// 按状态、订单号关键词分页倒序读取人工定价工单，并回填每单全部核实记录
/// （旧版 ListManualFeeReviews + ListMeterReviewsByOrder + MeterReviewService.List）。
/// 返回（本页行, 符合条件总数）。keyword 由调用方 trim 与限长，空串表示不筛选。
pub async fn list_manual_fee_reviews(
    db: &MySqlPool,
    status: &str,
    keyword: &str,
    page: u32,
    size: u32,
) -> Result<(Vec<ManualFeeReviewRow>, i64), anyhow::Error> {
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
    let count_sql = format!("SELECT COUNT(*) FROM `manual_fee_review` {where_sql}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        count_query = count_query.bind(b);
    }
    let total = count_query.fetch_one(db).await?;
    let offset = (page.max(1) as i64 - 1) * size.max(1) as i64;
    let list_sql = format!(
        "SELECT `charge_order_id`, CAST(`order_no` AS CHAR) AS `order_no`, \
                CAST(`reason` AS CHAR) AS `reason`, CAST(`status` AS CHAR) AS `status`, \
                CAST(`source_json` AS CHAR) AS `source_json`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `manual_fee_review` {where_sql} ORDER BY `id` DESC LIMIT ? OFFSET ?"
    );
    let mut list_query = sqlx::query_as::<_, ManualFeeReviewRawRow>(&list_sql);
    for b in &binds {
        list_query = list_query.bind(b);
    }
    list_query = list_query.bind(size.max(1)).bind(offset);
    let raws = list_query.fetch_all(db).await?;
    // 本页订单 ID 批量取核实记录（空页不查询），按订单分组、ID 倒序。
    let mut rows: Vec<ManualFeeReviewRow> = raws
        .into_iter()
        .map(|raw| ManualFeeReviewRow {
            charge_order_id: raw.charge_order_id,
            order_no: raw.order_no,
            reason: raw.reason,
            status: raw.status,
            source: raw
                .source_json
                .and_then(|s| serde_json::from_str(&s).ok())
                .map(decimalize_user_id),
            created_at: raw.created_at,
            reviews: Vec::new(),
        })
        .collect();
    let order_ids: Vec<u64> = rows.iter().map(|r| r.charge_order_id).collect();
    if !order_ids.is_empty() {
        let mut qb = sqlx::QueryBuilder::new(format!(
            "SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` \
             WHERE `charge_order_id` IN ("
        ));
        let mut separated = qb.separated(", ");
        for oid in &order_ids {
            separated.push_bind(*oid as i64);
        }
        qb.push(") ORDER BY `id` DESC");
        let reviews: Vec<MeterReviewRawRow> = qb.build_query_as().fetch_all(db).await?;
        let mut grouped: std::collections::HashMap<u64, Vec<MeterReviewRow>> =
            std::collections::HashMap::new();
        for review in reviews.into_iter().map(MeterReviewRawRow::into_row) {
            grouped
                .entry(review.charge_order_id)
                .or_default()
                .push(review);
        }
        for row in &mut rows {
            row.reviews = grouped.remove(&row.charge_order_id).unwrap_or_default();
        }
    }
    Ok((rows, total))
}

/// 把对象内的 user_id 数字字段转成字符串（旧版 DecimalizeIDField 语义）。
fn decimalize_user_id(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(id) = v.get("user_id").and_then(|x| x.as_u64())
        && let Some(obj) = v.as_object_mut()
    {
        obj.insert("user_id".into(), serde_json::Value::String(id.to_string()));
    }
    v
}

/// 关键词 LIKE 转义（旧版 dbutil.LikePattern：转义 ! % _，用 ! 作逃逸符）。
fn like_pattern(value: &str) -> String {
    format!(
        "%{}%",
        value
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    )
}

/// 人工复核域错误（语义对齐旧版 ErrBillingConflict 族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    /// 请求冲突：状态、内容或审核人不允许该操作。
    Conflict,
    /// 计量或快照无效（旧版 ErrMeterReview / ErrInvalidPricing）。
    InvalidSource,
    /// 记录不存在。
    NotFound,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewError::Conflict => f.write_str("人工复核请求冲突"),
            ReviewError::InvalidSource => f.write_str("计量或计费快照无效"),
            ReviewError::NotFound => f.write_str("记录不存在"),
            ReviewError::Storage(m) => write!(f, "人工复核存储错误: {m}"),
        }
    }
}

impl std::error::Error for ReviewError {}

impl From<sqlx::Error> for ReviewError {
    fn from(e: sqlx::Error) -> Self {
        ReviewError::Storage(e.to_string())
    }
}

/// 修正提案的计量分段输入（有单位与分段约束，不补造计量）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MeterSegmentInput {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub energy_wh: u32,
    pub peak_w: u32,
}

/// 原始计费源：快照 + 结束计量（旧版 readOriginalBillingSource 的对应部分）。
async fn read_billing_source(
    c: &mut sqlx::MySqlConnection,
    charge_order_id: u64,
) -> Result<(PricingSnapshot, EndMeter), ReviewError> {
    let snapshot_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` \
         WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
    .map_err(ReviewError::from)?;
    let meter_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
    .map_err(ReviewError::from)?;
    let (Some(snapshot_json), Some(meter_json)) = (snapshot_json, meter_json) else {
        return Err(ReviewError::NotFound);
    };
    let snapshot: PricingSnapshot =
        serde_json::from_str(&snapshot_json).map_err(|_| ReviewError::InvalidSource)?;
    let meter: EndMeter =
        serde_json::from_str(&meter_json).map_err(|_| ReviewError::InvalidSource)?;
    Ok((snapshot, meter))
}

/// 快照 + 计量构造 ActualMeter（与计费批次同构：started_at 取首段开始）。
fn actual_meter_of(meter: &EndMeter) -> ActualMeter {
    ActualMeter {
        review_required: false,
        started_at: meter.segments.first().map(|s| s.started_at),
        ended_at: Some(meter.ended_at),
        charged_wh: meter.charged_wh,
        charged_seconds: meter.charged_seconds,
        segments: meter.segments.clone(),
    }
}

/// 提交计量修正提案（旧版 ProposeMeter）：任务须 manual_review、无活跃提案；
/// 同 request_id 同内容重放复用首次记录；修正分段须经价格校验。
/// 首次提交时在事务内写审计（重放不写，旧版 prior 分支语义）。
#[allow(clippy::too_many_arguments)]
pub async fn propose_meter_review(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    actor_id: u64,
    charge_order_id: u64,
    request_id: &str,
    reason: &str,
    segments: &[MeterSegmentInput],
) -> Result<u64, ReviewError> {
    if reason.trim().is_empty() || reason.len() > 500 || segments.is_empty() {
        return Err(ReviewError::Conflict);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 锁定计费任务（顺序化人工出口）。
    let job: Option<BillingJobStatus> = sqlx::query_scalar(
        "SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(job_status) = job else {
        return Err(ReviewError::NotFound);
    };
    // 同 request_id 重放：内容一致复用首次记录。
    let prior: Option<(u64, u64, String, String)> = sqlx::query_as(
        "SELECT `id`, `charge_order_id`, `reason`, CAST(`corrected_json` AS CHAR) AS `corrected_json` \
         FROM `charge_meter_review` WHERE CAST(`request_id` AS CHAR) = ? FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await?;
    if let Some((review_id, prior_order, prior_reason, prior_corrected)) = prior {
        let consistent = prior_order == charge_order_id
            && prior_reason == reason
            && segments_match_json(&prior_corrected, segments);
        if !consistent {
            return Err(ReviewError::Conflict);
        }
        tx.commit().await?;
        return Ok(review_id);
    }
    if job_status != BillingJobStatus::ManualReview {
        return Err(ReviewError::Conflict);
    }
    // 活跃提案（awaiting_second/approved）存在时拒绝。
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_meter_review` \
         WHERE `charge_order_id` = ? AND `status` IN (?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(MeterReviewStatus::AwaitingSecond)
    .bind(MeterReviewStatus::Approved)
    .fetch_one(&mut *c)
    .await?;
    if active != 0 {
        return Err(ReviewError::Conflict);
    }
    // 原始源 + 修正分段的价格校验（不写回快照；批准重排队后由计费批次消费）。
    let (snapshot, meter) = read_billing_source(&mut *c, charge_order_id).await?;
    let corrected = EndMeter {
        segments: segments
            .iter()
            .map(|s| chargepilot_domain::charging_actual::MeterSegment {
                started_at: s.started_at,
                ended_at: s.ended_at,
                energy_wh: s.energy_wh,
                peak_w: s.peak_w,
                power_w: None,
            })
            .collect(),
        ..meter.clone()
    };
    // 修正后合计必须与总量一致（usage_from_meter 的分段守恒校验）。
    let actual = actual_meter_of(&corrected);
    let rule = Rule {
        id: 1,
        version: 1,
        spec: snapshot.rule.spec.clone(),
        ..Default::default()
    };
    price_actual(&rule, &actual).map_err(|_| ReviewError::InvalidSource)?;
    let original_json = serde_json::json!({
        "meter": meter,
    });
    let corrected_json = serde_json::json!({
        "meter": corrected,
    });
    sqlx::query(
        "INSERT INTO `charge_meter_review` (`request_id`, `charge_order_id`, `original_json`, \
         `corrected_json`, `reason`, `status`, `first_reviewer_id`) \
         VALUES (?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, ?, ?)",
    )
    .bind(request_id)
    .bind(charge_order_id as i64)
    .bind(original_json.to_string())
    .bind(corrected_json.to_string())
    .bind(reason)
    .bind(MeterReviewStatus::AwaitingSecond)
    .bind(actor_id as i64)
    .execute(&mut *c)
    .await?;
    let review_id: u64 = sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await?;
    // 首次提交审计（旧版 AuditChange{Action:"propose", Target:"meter_review", After:in}）。
    if let Some(audit) = audit {
        let after = serde_json::json!({
            "request_id": request_id,
            "reason": reason,
            "segments": segments,
        });
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            audit.actor,
            "meter_review",
            "propose",
            "meter_review",
            &review_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit.client_ip,
            request_id,
        )
        .await
        .map_err(|e| ReviewError::Storage(e.to_string()))?;
    }
    tx.commit().await?;
    Ok(review_id)
}

/// 校验已存 corrected_json 的分段与输入一致（旧版 SameSegments + JSON 比对）。
fn segments_match_json(corrected_json: &str, segments: &[MeterSegmentInput]) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(corrected_json) else {
        return false;
    };
    let Some(stored) = v["meter"]["segments"].as_array() else {
        return false;
    };
    if stored.len() != segments.len() {
        return false;
    }
    stored.iter().zip(segments).all(|(s, m)| {
        s["energy_wh"].as_u64() == Some(m.energy_wh as u64)
            && s["peak_w"].as_u64() == Some(m.peak_w as u64)
    })
}

/// 完成计量审核（旧版 ReviewMeter）：仅 awaiting_second 可决定，二审须非首审人。
/// 批准核对原始源未变且修正可计价，重排队计费（generation+1）；驳回记录依据。
/// 首次决定时在事务内写审计（重放不写，旧版 Decide 分支语义）。
#[allow(clippy::too_many_arguments)]
pub async fn review_meter(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    actor_id: u64,
    charge_order_id: u64,
    review_id: u64,
    approve: bool,
    reason: &str,
    first_reviewer_permitted: Option<bool>,
) -> Result<(), ReviewError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let job: Option<BillingJobStatus> = sqlx::query_scalar(
        "SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(job_status) = job else {
        return Err(ReviewError::NotFound);
    };
    // 审前行快照（旧版 FindMeterReview + Before=review 的审计语义）。
    let before_sql = format!(
        "SELECT {METER_REVIEW_COLUMNS} FROM `charge_meter_review` \
         WHERE `id` = ? AND `charge_order_id` = ? FOR UPDATE"
    );
    let before_row: Option<MeterReviewRawRow> = sqlx::query_as(&before_sql)
        .bind(review_id as i64)
        .bind(charge_order_id as i64)
        .fetch_optional(&mut *c)
        .await?;
    let Some(before_raw) = before_row else {
        return Err(ReviewError::NotFound);
    };
    let corrected_json = before_raw.corrected_json.clone();
    let review_status = before_raw.status;
    let first_reviewer = before_raw.first_reviewer_id;
    let second_reviewer = before_raw.second_reviewer_id;
    // 审计 before 取审前完整行（original/corrected 解析为对象，对齐旧版 MeterReview JSON）。
    let before_json =
        serde_json::to_value(before_raw.clone().into_row()).unwrap_or(serde_json::Value::Null);
    // 相同决定重试（旧版幂等分支）。
    let target = if approve {
        MeterReviewStatus::Approved
    } else {
        MeterReviewStatus::Rejected
    };
    if review_status == target && second_reviewer == Some(actor_id) {
        if !approve && reason.trim().is_empty() {
            return Err(ReviewError::Conflict);
        }
        tx.commit().await?;
        return Ok(());
    }
    if job_status != BillingJobStatus::ManualReview
        || review_status != MeterReviewStatus::AwaitingSecond
        || first_reviewer == actor_id
    {
        return Err(ReviewError::Conflict);
    }
    if approve {
        // 原始源未变 + 修正分段可计价。
        let (snapshot, meter) = read_billing_source(&mut *c, charge_order_id).await?;
        let corrected: serde_json::Value =
            serde_json::from_str(&corrected_json).map_err(|_| ReviewError::InvalidSource)?;
        let corrected_meter: EndMeter = serde_json::from_value(corrected["meter"].clone())
            .map_err(|_| ReviewError::InvalidSource)?;
        // 校验修正仅替换分段（其余计量事实不变）。
        if corrected_meter.charged_wh != meter.charged_wh
            || corrected_meter.charged_seconds != meter.charged_seconds
            || corrected_meter.ended_at != meter.ended_at
        {
            return Err(ReviewError::Conflict);
        }
        let actual = actual_meter_of(&corrected_meter);
        let rule = Rule {
            id: 1,
            version: 1,
            spec: snapshot.rule.spec.clone(),
            ..Default::default()
        };
        price_actual(&rule, &actual).map_err(|_| ReviewError::InvalidSource)?;
        // 首审人当前权限：None 由事务内一致读取实时查询（HTTP 语义），
        // Some(v) 为调用方直接注入（模块级测试）。
        let permitted = match first_reviewer_permitted {
            Some(v) => v,
            None => {
                let profile = chargepilot_mod_ops::repo::load_profile_tx(&mut *c, first_reviewer)
                    .await
                    .map_err(|_| ReviewError::Conflict)?;
                profile.role == "customer_finance"
                    && profile
                        .permissions
                        .iter()
                        .any(|p| p == "billing.meter.review")
            }
        };
        if !permitted {
            return Err(ReviewError::Conflict);
        }
    }
    if approve {
        sqlx::query(
            "UPDATE `charge_meter_review` SET `status` = ?, `second_reviewer_id` = ?, \
             `reviewed_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
        )
        .bind(MeterReviewStatus::Approved)
        .bind(actor_id as i64)
        .bind(review_id as i64)
        .execute(&mut *c)
        .await?;
        // 重排队计费：修正分段参与重算（generation 递增防旧派发覆盖）。
        sqlx::query(
            "UPDATE `charge_billing_job` SET `status` = ?, `generation` = `generation` + 1, \
             `next_attempt_at` = UTC_TIMESTAMP(3), `last_error` = NULL WHERE `charge_order_id` = ?",
        )
        .bind(BillingJobStatus::Pending)
        .bind(charge_order_id as i64)
        .execute(&mut *c)
        .await?;
    } else {
        if reason.trim().is_empty() {
            return Err(ReviewError::Conflict);
        }
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
    }
    // 首次决定审计（旧版 AuditChange{Action:"decide", Target:"meter_review", Before:review, After:in}）。
    if let Some(audit) = audit {
        let after = serde_json::json!({
            "review_id": review_id,
            "approve": approve,
            "reason": reason,
        });
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            audit.actor,
            "meter_review",
            "decide",
            "meter_review",
            &review_id.to_string(),
            &before_json,
            &after,
            audit.client_ip,
            audit.request_id,
        )
        .await
        .map_err(|e| ReviewError::Storage(e.to_string()))?;
    }
    tx.commit().await?;
    Ok(())
}

/// 登记人工最终金额（旧版 ResolveAmount）：任务须 manual_review，一单一金额，
/// 同 request_id 重放核对内容；合计不超过套餐价格；重排队按人工金额入账。
/// 首次登记时在事务内写审计（重放不写）。
#[allow(clippy::too_many_arguments)]
pub async fn resolve_manual_amount(
    pool: &MySqlPool,
    audit: Option<&AuditView<'_>>,
    actor_id: u64,
    charge_order_id: u64,
    request_id: &str,
    electric_cents: i64,
    service_cents: i64,
    reason: &str,
) -> Result<(), ReviewError> {
    if actor_id == 0
        || reason.trim().is_empty()
        || reason.len() > 1000
        || electric_cents < 0
        || service_cents < 0
        || electric_cents > 1_000_000
        || service_cents > 1_000_000
    {
        return Err(ReviewError::Conflict);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let job: Option<BillingJobStatus> = sqlx::query_scalar(
        "SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(job_status) = job else {
        return Err(ReviewError::NotFound);
    };
    let prior: Option<(u64, String, i64, i64, String)> = sqlx::query_as(
        "SELECT `charge_order_id`, CAST(`request_id` AS CHAR) AS `request_id`, \
                `electric_cents`, `service_cents`, `reason` \
         FROM `charge_manual_settlement` \
         WHERE `charge_order_id` = ? OR `request_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .bind(request_id)
    .fetch_optional(&mut *c)
    .await?;
    if let Some((p_order, p_request, p_electric, p_service, p_reason)) = prior {
        if p_order != charge_order_id
            || p_request != request_id
            || p_electric != electric_cents
            || p_service != service_cents
            || p_reason != reason
        {
            return Err(ReviewError::Conflict);
        }
        tx.commit().await?;
        return Ok(());
    }
    if job_status != BillingJobStatus::ManualReview {
        return Err(ReviewError::Conflict);
    }
    // 合计不超过套餐价格（快照 offer）。
    let snapshot_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` \
         WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(snapshot_json) = snapshot_json else {
        return Err(ReviewError::NotFound);
    };
    let snapshot: PricingSnapshot =
        serde_json::from_str(&snapshot_json).map_err(|_| ReviewError::InvalidSource)?;
    if electric_cents + service_cents > snapshot.offer.price_cents {
        return Err(ReviewError::Conflict);
    }
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
    chargepilot_mod_order::append_event_in_tx(
        &mut *c,
        charge_order_id,
        request_id,
        "manual_settlement",
        "admin",
        reason,
        chrono::Utc::now().timestamp_micros(),
    )
    .await
    .map_err(|e| ReviewError::Storage(e.to_string()))?;
    sqlx::query(
        "UPDATE `charge_billing_job` SET `status` = ?, `generation` = `generation` + 1, \
         `last_error` = NULL, `next_attempt_at` = UTC_TIMESTAMP(3) WHERE `charge_order_id` = ?",
    )
    .bind(BillingJobStatus::Pending)
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    // 首次登记审计（旧版 AuditChange{Action:"resolve_amount", Target:"manual_settlement", After:in}）。
    if let Some(audit) = audit {
        let after = serde_json::json!({
            "charge_order_id": charge_order_id,
            "request_id": request_id,
            "actor_id": actor_id,
            "electric_cents": electric_cents,
            "service_cents": service_cents,
            "reason": reason,
        });
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            audit.actor,
            "manual_settlement",
            "resolve_amount",
            "manual_settlement",
            &charge_order_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit.client_ip,
            request_id,
        )
        .await
        .map_err(|e| ReviewError::Storage(e.to_string()))?;
    }
    tx.commit().await?;
    Ok(())
}
