//! 计量核实（仪表审核）与人工最终金额（旧版 meter_review_repo.go + manual_repo.go）。
//! 计费任务 manual_review 后的两条人工出口：
//! 1. 修正计量分段（双人审核，批准后重排队计费重算）；
//! 2. 直接登记最终金额（合计不超过套餐价格，重排队按人工金额入账）。
//! 权限与审计由 HTTP 层负责；首审人实时权限由调用方传入。

use chargepilot_domain::charging_actual::{ActualMeter, EndMeter, Rule};
use chargepilot_domain::charging_actual::price_actual;
use chargepilot_mod_payment::checkout::PricingSnapshot;
use sqlx::{Acquire, MySqlPool};

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
#[derive(Debug, Clone)]
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
    let snapshot: PricingSnapshot = serde_json::from_str(&snapshot_json)
        .map_err(|_| ReviewError::InvalidSource)?;
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
pub async fn propose_meter_review(
    pool: &MySqlPool,
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
    let job: Option<String> = sqlx::query_scalar(
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
    if job_status != crate::billing::JobStatus::MANUAL_REVIEW {
        return Err(ReviewError::Conflict);
    }
    // 活跃提案（awaiting_second/approved）存在时拒绝。
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_meter_review` \
         WHERE `charge_order_id` = ? AND `status` IN ('awaiting_second', 'approved')",
    )
    .bind(charge_order_id as i64)
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
         VALUES (?, ?, CAST(? AS JSON), CAST(? AS JSON), ?, 'awaiting_second', ?)",
    )
    .bind(request_id)
    .bind(charge_order_id as i64)
    .bind(original_json.to_string())
    .bind(corrected_json.to_string())
    .bind(reason)
    .bind(actor_id as i64)
    .execute(&mut *c)
    .await?;
    let review_id: u64 = sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await?;
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
pub async fn review_meter(
    pool: &MySqlPool,
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
    let job: Option<String> = sqlx::query_scalar(
        "SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(job_status) = job else {
        return Err(ReviewError::NotFound);
    };
    let review: Option<(String, String, u64, Option<u64>)> = sqlx::query_as(
        "SELECT `status`, CAST(`corrected_json` AS CHAR), `first_reviewer_id`, `second_reviewer_id` \
         FROM `charge_meter_review` WHERE `id` = ? AND `charge_order_id` = ? FOR UPDATE",
    )
    .bind(review_id as i64)
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((review_status, corrected_json, first_reviewer, second_reviewer)) = review else {
        return Err(ReviewError::NotFound);
    };
    // 相同决定重试（旧版幂等分支）。
    let target = if approve { "approved" } else { "rejected" };
    if review_status == target && second_reviewer == Some(actor_id) {
        if !approve && reason.trim().is_empty() {
            return Err(ReviewError::Conflict);
        }
        tx.commit().await?;
        return Ok(());
    }
    if job_status != crate::billing::JobStatus::MANUAL_REVIEW
        || review_status != "awaiting_second"
        || first_reviewer == actor_id
    {
        return Err(ReviewError::Conflict);
    }
    if approve {
        // 原始源未变 + 修正分段可计价。
        let (snapshot, meter) = read_billing_source(&mut *c, charge_order_id).await?;
        let corrected: serde_json::Value = serde_json::from_str(&corrected_json)
            .map_err(|_| ReviewError::InvalidSource)?;
        let corrected_meter: EndMeter =
            serde_json::from_value(corrected["meter"].clone())
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
        if first_reviewer_permitted != Some(true) {
            return Err(ReviewError::Conflict);
        }
    }
    if approve {
        sqlx::query(
            "UPDATE `charge_meter_review` SET `status` = 'approved', `second_reviewer_id` = ?, \
             `reviewed_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
        )
        .bind(actor_id as i64)
        .bind(review_id as i64)
        .execute(&mut *c)
        .await?;
        // 重排队计费：修正分段参与重算（generation 递增防旧派发覆盖）。
        sqlx::query(
            "UPDATE `charge_billing_job` SET `status` = ?, `generation` = `generation` + 1, \
             `next_attempt_at` = UTC_TIMESTAMP(3), `last_error` = NULL WHERE `charge_order_id` = ?",
        )
        .bind(crate::billing::JobStatus::PENDING)
        .bind(charge_order_id as i64)
        .execute(&mut *c)
        .await?;
    } else {
        if reason.trim().is_empty() {
            return Err(ReviewError::Conflict);
        }
        sqlx::query(
            "UPDATE `charge_meter_review` SET `status` = 'rejected', `second_reviewer_id` = ?, \
             `reviewed_at` = UTC_TIMESTAMP(3), `reject_reason` = ? WHERE `id` = ?",
        )
        .bind(actor_id as i64)
        .bind(reason)
        .bind(review_id as i64)
        .execute(&mut *c)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 登记人工最终金额（旧版 ResolveAmount）：任务须 manual_review，一单一金额，
/// 同 request_id 重放核对内容；合计不超过套餐价格；重排队按人工金额入账。
pub async fn resolve_manual_amount(
    pool: &MySqlPool,
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
    let job: Option<String> = sqlx::query_scalar(
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
    if job_status != crate::billing::JobStatus::MANUAL_REVIEW {
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
    let snapshot: PricingSnapshot = serde_json::from_str(&snapshot_json)
        .map_err(|_| ReviewError::InvalidSource)?;
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
    .bind(crate::billing::JobStatus::PENDING)
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    tx.commit().await?;
    Ok(())
}
