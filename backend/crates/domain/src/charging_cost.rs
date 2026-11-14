//! Cost 计费引擎：分时段功率/电量计价、费用片段与计量截止冻结。
//! 语义基准：旧仓库 internal/domain/charging/{cost,scheme_cost,breakdown}.go，逐行对齐。

use crate::charging_spec::{
    BillingError, ChargeMode, MAX_SLICES, Spec, Usage, beijing, compile_periods, div_prec16,
    round_to_cents, string_fixed6, validate_spec,
};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 计价结果：Basis 保留计价规则供展示单位（旧版 Fee）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Fee {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<FeeFragment>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub basis: String,
    pub electric_cents: i64,
    pub service_cents: i64,
    pub total_cents: i64,
    pub billable_wh: u64,
}

/// 区间用量、单价和取整前费用，用于解释 Cost 的合计结果（旧版 FeeFragment）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeeFragment {
    pub started_at: String,
    pub ended_at: String,
    pub power_w: u32,
    /// 固定 6 位小数的数量文本（shopspring StringFixed(6) 契约）。
    pub quantity: String,
    pub unit: String,
    pub electric_rate: i64,
    pub service_rate: i64,
    pub electric_cents: String,
    pub service_cents: String,
}

/// 时间输出格式与旧版 JSON 序列化对齐（RFC3339 纳秒、UTC Z 后缀）。
fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

#[allow(clippy::too_many_arguments)]
fn fragment(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    power: u32,
    q: Decimal,
    unit: &str,
    e: i64,
    s: i64,
) -> FeeFragment {
    let e_dec = Decimal::from(e);
    let s_dec = Decimal::from(s);
    FeeFragment {
        started_at: fmt_time(start),
        ended_at: fmt_time(end),
        power_w: power,
        quantity: string_fixed6(q),
        unit: unit.to_string(),
        electric_rate: e,
        service_rate: s,
        electric_cents: string_fixed6(q * e_dec),
        service_cents: string_fixed6(q * s_dec),
    }
}

/// 阶梯选档：功率落在 [上一档上界+1, 本档上界] 用本档；超出最后一档用最后一档。
pub fn tier_for(period: &crate::charging_spec::Period, watts: u32) -> crate::charging_spec::Tier {
    let mut lower = 0u64;
    for tier in &period.tiers {
        if (watts as u64) >= lower && (watts as u64) <= tier.max_watts.max(0) as u64 {
            return tier.clone();
        }
        lower = tier.max_watts.max(0) as u64 + 1;
    }
    period
        .tiers
        .last()
        .cloned()
        .unwrap_or(crate::charging_spec::Tier {
            max_watts: 0,
            electric_cents: 0,
            service_cents: 0,
        })
}

/// 时刻所属费率时段；配置非法时返回零值时段（调用方校验在先，与旧版一致）。
pub fn period_at(spec: &Spec, at: DateTime<Utc>) -> crate::charging_spec::Period {
    let Some(electric) = &spec.electric else {
        return Default::default();
    };
    let Some(schedule) = compile_periods(&electric.periods) else {
        return Default::default();
    };
    let local = at.with_timezone(&beijing());
    let idx = (local.hour() * 60 + local.minute()) as usize;
    schedule[idx].clone()
}

use chrono::Timelike;

/// Cost：按 spec 和 usage 计算电费、服务费，合计后各自四舍五入到分。
/// 功率计费只收完整分钟；规则无效、非服务端计费或计量不足时返回错误（旧版 costUsage）。
pub fn cost(spec: &Spec, usage: &Usage) -> Result<Fee, BillingError> {
    validate_spec(spec)?;
    if !spec.mode.server_billed() {
        return Err(BillingError::NotServerBilled);
    }
    let (start, end) = match (usage.start, usage.end) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err(BillingError::InvalidPricing), // 旧版 IsZero 检查
    };
    if end < start || end - start > Duration::days(7) {
        return Err(BillingError::InvalidPricing);
    }
    let basis = spec
        .mode
        .basis_for()
        .map(|b| b.as_str().to_string())
        .unwrap_or_default();
    let mut fee = Fee {
        basis,
        ..Default::default()
    };
    let minutes = usage.minutes();
    if usage.energy_wh == 0 || (spec.free_minutes > 0 && minutes <= spec.free_minutes as i64) {
        return Ok(fee);
    }
    let end_full_minutes = start + Duration::minutes(minutes);
    // 采样点五元组（起止时刻、电量 Wh、功率 W、功率是否实测）——type_complexity 要求具名。
    type SampleTuple = (DateTime<Utc>, DateTime<Utc>, u64, u32, bool);
    let mut samples: Vec<SampleTuple> = usage
        .samples
        .iter()
        .map(|s| (s.start, s.end, s.energy_wh, s.power_w, s.power_known))
        .collect();
    if samples.is_empty() {
        samples = vec![(start, end, usage.energy_wh, 0, false)];
    }
    if samples.len() > MAX_SLICES {
        return Err(BillingError::MeterReview);
    }
    let mut cursor = start;
    let mut total_wh = 0u64;
    let mut electric = Decimal::ZERO;
    let mut service = Decimal::ZERO;
    // 最大功率聚合：key = 北京日期 + 时段边界时间（旧版 map key "2006-01-02"+"15:04"）
    struct PeakPeriod {
        start: DateTime<Utc>,
        time_end: DateTime<Utc>,
        minutes: Decimal,
        peak: u32,
        period: crate::charging_spec::Period,
    }
    let mut peaks: HashMap<String, PeakPeriod> = HashMap::new();
    let bj = beijing();
    for (sample_start, sample_end, sample_wh, sample_power, power_known) in &samples {
        if *sample_start != cursor || sample_end < sample_start || sample_end > &end {
            return Err(BillingError::MeterReview);
        }
        cursor = *sample_end;
        total_wh += sample_wh;
        if spec.mode == ChargeMode::new(ChargeMode::SERVER_ENERGY) {
            let uniform_end = *sample_end - Duration::nanoseconds(1);
            if !crate::charging_spec::spec_is_uniform_over(spec, *sample_start, uniform_end) {
                return Err(BillingError::MeterReview);
            }
            let period = period_at(spec, *sample_start);
            let kwh = div_prec16(Decimal::from(*sample_wh as i64), Decimal::from(1000));
            fee.fragments.push(fragment(
                *sample_start,
                *sample_end,
                0,
                kwh,
                "度",
                period.electric_cents,
                period.service_cents,
            ));
            electric += kwh * Decimal::from(period.electric_cents);
            service += kwh * Decimal::from(period.service_cents);
            continue;
        }
        if *sample_power == 0 && !power_known {
            return Err(BillingError::MeterReview);
        }
        let mut at = *sample_start;
        while at < *sample_end && at < end_full_minutes {
            let period = period_at(spec, at);
            let local = at.with_timezone(&bj);
            // 北京无夏令时，当日零点恒存在。
            let day_start = match bj
                .with_ymd_and_hms(local.year(), local.month(), local.day(), 0, 0, 0)
                .single()
            {
                Some(d) => d,
                None => panic!("北京时区当日零点恒存在"),
            };
            let boundary = (day_start + Duration::minutes(period.end_minute)).with_timezone(&Utc);
            let mut next = *sample_end;
            if next > end_full_minutes {
                next = end_full_minutes;
            }
            if next > boundary {
                next = boundary;
            }
            let minutes_dec = div_prec16(
                Decimal::from((next - at).num_nanoseconds().unwrap_or(0)),
                Decimal::from(60_000_000_000i64),
            );
            if spec.mode == ChargeMode::new(ChargeMode::SERVER_MAX_POWER) {
                let key = format!(
                    "{} {}",
                    local.format("%Y-%m-%d"),
                    boundary.with_timezone(&bj).format("%H:%M")
                );
                let v = peaks.entry(key).or_insert_with(|| PeakPeriod {
                    start: at,
                    time_end: next,
                    minutes: Decimal::ZERO,
                    peak: 0,
                    period: period.clone(),
                });
                v.time_end = next;
                v.period = period.clone();
                v.minutes += minutes_dec;
                if *sample_power > v.peak {
                    v.peak = *sample_power;
                }
            } else {
                let tier = tier_for(&period, *sample_power);
                fee.fragments.push(fragment(
                    at,
                    next,
                    *sample_power,
                    div_prec16(minutes_dec, Decimal::from(60)),
                    "小时",
                    tier.electric_cents,
                    tier.service_cents,
                ));
                electric += div_prec16(
                    Decimal::from(tier.electric_cents) * minutes_dec,
                    Decimal::from(60),
                );
                service += div_prec16(
                    Decimal::from(tier.service_cents) * minutes_dec,
                    Decimal::from(60),
                );
            }
            at = next;
        }
    }
    if cursor != end || total_wh != usage.energy_wh {
        return Err(BillingError::MeterReview);
    }
    for v in peaks.values() {
        let tier = tier_for(&v.period, v.peak);
        fee.fragments.push(fragment(
            v.start,
            v.time_end,
            v.peak,
            div_prec16(v.minutes, Decimal::from(60)),
            "小时",
            tier.electric_cents,
            tier.service_cents,
        ));
        electric += div_prec16(
            Decimal::from(tier.electric_cents) * v.minutes,
            Decimal::from(60),
        );
        service += div_prec16(
            Decimal::from(tier.service_cents) * v.minutes,
            Decimal::from(60),
        );
    }
    fee.fragments.sort_by_key(|f| f.started_at.clone());
    fee.electric_cents = round_to_cents(electric).ok_or(BillingError::InvalidPricing)?;
    fee.service_cents = round_to_cents(service).ok_or(BillingError::InvalidPricing)?;
    if minutes > 0 && fee.electric_cents < spec.min_electric_cents {
        fee.electric_cents = spec.min_electric_cents;
    }
    fee.total_cents = fee.electric_cents + fee.service_cents;
    fee.billable_wh = usage.energy_wh;
    Ok(fee)
}

/// 返回截至 cutoff 的计量；截止点不早于结束时原样返回，早于开始时返回 MeterReview。
/// 电量计费须有截止读数；功率计费须有该段功率，最大功率还要求该段峰值不高于该功率
/// （旧版 CutoffMeter；ActualMeter 定义见 charging_actual）。
pub fn cutoff_meter(
    spec: &Spec,
    m: &crate::charging_actual::ActualMeter,
    cutoff: DateTime<Utc>,
) -> Result<crate::charging_actual::ActualMeter, BillingError> {
    use crate::charging_spec::ChargeMode as CM;
    let ended = m.ended_at.ok_or(BillingError::MeterReview)?;
    if cutoff >= ended {
        return Ok(m.clone());
    }
    let started = m.started_at.ok_or(BillingError::MeterReview)?;
    if cutoff < started {
        return Err(BillingError::MeterReview);
    }
    if spec.mode == ChargeMode::new(CM::DEVICE_DURATION) {
        let mut m2 = m.clone();
        let seconds = (cutoff - started).num_seconds().max(0) as u32;
        if m2.charged_seconds > seconds {
            m2.charged_seconds = seconds;
        }
        m2.ended_at = Some(cutoff);
        return Ok(m2);
    }
    let mut segments = Vec::new();
    let mut wh = 0u32;
    for s in &m.segments {
        if s.started_at >= cutoff {
            break;
        }
        let mut s = s.clone();
        if s.ended_at > cutoff {
            let is_energy = spec.mode == ChargeMode::new(CM::SERVER_ENERGY)
                || spec.mode == ChargeMode::new(CM::DEVICE_ENERGY);
            if is_energy {
                return Err(BillingError::MeterReview);
            }
            let is_max_power = spec.mode == ChargeMode::new(CM::SERVER_MAX_POWER);
            if s.power_w.is_none() || (is_max_power && s.peak_w > s.power_w.unwrap_or(0)) {
                return Err(BillingError::MeterReview);
            }
            // 仅缩短功率计费区间，保留该段电量原值，不按时长估算电量。
            s.ended_at = cutoff;
        }
        wh += s.energy_wh;
        segments.push(s);
    }
    if segments.last().map(|s| s.ended_at) != Some(cutoff) {
        return Err(BillingError::MeterReview);
    }
    let mut m2 = m.clone();
    m2.segments = segments;
    m2.charged_wh = wh;
    m2.ended_at = Some(cutoff);
    m2.charged_seconds = (cutoff - started).num_seconds().max(0) as u32;
    Ok(m2)
}
