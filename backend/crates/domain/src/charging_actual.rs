//! 实测计量：分段电量/功率证据、计价入口与时长/费用上限判断。
//! 语义基准：旧仓库 internal/domain/charging/{actual,segments,end_meter}.go，逐行对齐。

use crate::charging_cost::cost;
use crate::charging_spec::{
    spec_is_uniform_over, validate_spec, BillingError, ChargeMode, MAX_SLICES, Spec, Usage,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// 连续时间段的电量和功率；相邻段必须首尾相接（旧版 MeterSegment）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeterSegment {
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// 本段电量，Wh。
    pub energy_wh: u32,
    /// 该段峰值功率，W；最大功率计费使用此值，不由电量推算。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub peak_w: u32,
    /// 该段实测功率，W；None 表示缺失，Some(0) 表示已知零功率。
    /// 实时功率计费要求此值，不能用峰值或电量替代。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_w: Option<u32>,
}

/// 一次充电的实测总量与分段；review_required 为真时拒绝自动计费（旧版 ActualMeter）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActualMeter {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review_required: bool,
    /// 时间为零值表示缺失（旧版 IsZero 判定）；chrono 无零值时刻，用 Option 表达。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub charged_wh: u32,
    pub charged_seconds: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<MeterSegment>,
}

/// 设备结束回执的分段、总电量（Wh）、总时长（秒）和结束时刻（旧版 EndMeter）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EndMeter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<MeterSegment>,
    pub charged_wh: u32,
    pub charged_seconds: u32,
    pub ended_at: DateTime<Utc>,
    pub stop_reason: u8,
}

/// 逐段比较时间、电量、峰值和可选功率；None 与空列表视为相同（旧版 SameSegments）。
pub fn same_segments(a: &[MeterSegment], b: &[MeterSegment]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.energy_wh == y.energy_wh
            && x.peak_w == y.peak_w
            && x.power_w.is_none() == y.power_w.is_none()
            && x.power_w == y.power_w
            && x.started_at == y.started_at
            && x.ended_at == y.ended_at
    })
}

/// 已发布且关联站点的计费规则；预估与结算复用同一份规则及 Cost 算法（旧版 Rule）。
/// serde 字段名与旧版 JSON tag 一致（rule_id / station_id）——快照与事件载荷共用此形态。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(rename = "rule_id", default)]
    pub id: u64,
    #[serde(default)]
    pub station_id: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub spec: Spec,
    /// 规则默认的充电启动入口（旧版 channel，omitempty）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
}

/// usage_from_meter 校验计量记录并转换为 Cost 所需的 Usage。
/// 费用封顶与最终结算共用此校验，保持计量有效性及计费结果一致（旧版 usageFromMeter）。
pub fn usage_from_meter(rule: &Rule, meter: &ActualMeter) -> Result<Usage, BillingError> {
    if meter.review_required {
        return Err(BillingError::MeterReview);
    }
    if validate_spec(&rule.spec).is_err() || rule.id == 0 || rule.version == 0 {
        return Err(BillingError::InvalidPricing);
    }
    let started_at = match meter.started_at {
        Some(t) => t,
        None => return Err(BillingError::MeterReview), // 旧版 IsZero
    };
    let ended_at = match meter.ended_at {
        Some(t) => t,
        None => return Err(BillingError::MeterReview),
    };
    if ended_at < started_at
        || ended_at - started_at > Duration::days(7)
        || Duration::seconds(meter.charged_seconds as i64)
            > ended_at - started_at + Duration::minutes(5)
    {
        return Err(BillingError::MeterReview);
    }
    let mut segments: Vec<MeterSegment> = meter.segments.clone();
    if segments.is_empty() {
        segments = vec![MeterSegment {
            started_at,
            ended_at,
            energy_wh: meter.charged_wh,
            peak_w: 0,
            power_w: None,
        }];
    }
    if segments.len() > MAX_SLICES {
        return Err(BillingError::MeterReview);
    }
    let mut usage = Usage {
        start: Some(started_at),
        end: Some(ended_at),
        energy_wh: meter.charged_wh as u64,
        samples: Vec::with_capacity(segments.len()),
        channel: None,
    };
    // 费率不变时可直接按总电量计价；费率变化时须有边界读数，不能按时长分摊电量。
    if meter.segments.is_empty()
        && !spec_is_uniform_over(&rule.spec, started_at, ended_at)
    {
        return Err(BillingError::MeterReview);
    }
    let mut total_wh = 0u64;
    let mut cursor = started_at;
    for segment in &segments {
        if segment.started_at != cursor
            || segment.ended_at < segment.started_at
            || segment.ended_at > ended_at
            || (segment.started_at == segment.ended_at && segment.energy_wh > 0)
        {
            return Err(BillingError::MeterReview);
        }
        cursor = segment.ended_at;
        total_wh += segment.energy_wh as u64;
        let mut power = segment.peak_w;
        if rule.spec.mode == ChargeMode::new(ChargeMode::SERVER_REALTIME_POWER) {
            power = segment.power_w.ok_or(BillingError::MeterReview)?;
        }
        if rule.spec.mode == ChargeMode::new(ChargeMode::SERVER_MAX_POWER)
            && power == 0
            && segment.power_w.is_none()
            && meter.charged_wh > 0
        {
            return Err(BillingError::MeterReview);
        }
        usage.samples.push(crate::charging_spec::Sample {
            start: segment.started_at,
            end: segment.ended_at,
            energy_wh: segment.energy_wh as u64,
            power_w: power,
            power_known: segment.power_w.is_some(),
        });
    }
    if cursor != ended_at || total_wh != meter.charged_wh as u64 {
        return Err(BillingError::MeterReview);
    }
    Ok(usage)
}

/// 按 rule 计算实测费用；规则无效返回 InvalidPricing，计量缺失或矛盾返回 MeterReview。
/// 电量计费跨越不同费率时必须有分段读数，不按时长分配电量（旧版 PriceActual）。
pub fn price_actual(rule: &Rule, meter: &ActualMeter) -> Result<crate::charging_cost::Fee, BillingError> {
    let usage = usage_from_meter(rule, meter)?;
    cost(&rule.spec, &usage)
}

/// 停止计划：运行中服务端计费的停止决策（旧版 StopPlan，定义在 device.go）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StopPlan {
    pub should_stop: bool,
    pub reason: String,
    /// 判断时的累计总费用，单位分。
    pub accrued_cents: i64,
    /// 费用封顶后建议等待的秒数；本结构不确认停机结果。
    pub grace_seconds: i32,
}

/// 校验 meter 后判断是否达到 rule 的时长或费用上限。
/// 规则或计量无效时返回错误；设备计费返回无需平台停机的计划（旧版 StopAtMeter）。
pub fn stop_at_meter(rule: &Rule, meter: &ActualMeter) -> Result<StopPlan, BillingError> {
    let usage = usage_from_meter(rule, meter)?;
    let started_at = meter.started_at.ok_or(BillingError::MeterReview)?;
    let ended_at = meter.ended_at.ok_or(BillingError::MeterReview)?;
    let elapsed = ended_at - started_at;
    crate::charging_settle::decide_stop(&rule.spec, &usage, elapsed)
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
