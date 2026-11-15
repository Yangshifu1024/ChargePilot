//! 结算与停止决策：SettleSession 金额拆分、预算停机、购买报价与套餐 Offer。
//! 语义基准：旧仓库 internal/domain/charging/{device,offer,offer_actual,budget_stop,estimate}.go，逐行对齐。

use crate::charging::actual::{ActualMeter, Rule, StopPlan, price_actual};
use crate::charging::cost::cost;
use crate::charging::spec::{
    BillingError, ChargeExecutor, ChargeMode, MAX_RATE_CENTS, MAX_SLICES, Spec, Usage, div_prec16,
    energy_pattern_ok, int_part, parse_energy, validate_spec,
};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 设备上报的停止原因；未知编码保留原值（旧版 StopReason 词表）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReason(pub String);

impl StopReason {
    pub const EXHAUSTED_TIME: &'static str = "exhausted_time";
    pub const EXHAUSTED_ENERGY: &'static str = "exhausted_energy";
    pub const CHARGER_REMOVED: &'static str = "charger_removed";
    pub const BATTERY_FULL: &'static str = "battery_full";
    pub const FAULT: &'static str = "fault";
    pub const POWER_TOO_HIGH: &'static str = "power_too_high";
    pub const CARD_REFUND: &'static str = "card_refund";
    pub const NO_CHARGER: &'static str = "no_charger";
    pub const REMOTE: &'static str = "remote";
    pub const SMOKE: &'static str = "smoke";
    pub const OVERHEAT: &'static str = "overheat";
    pub const SERVER_DISCONNECT: &'static str = "server_disconnect";
    pub const USER: &'static str = "user";
    pub const UNKNOWN: &'static str = "unknown";
}

/// 支付后下发的设备控制参数；仅当前模式需要的字段有效（旧版 ControlInstruction）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ControlInstruction {
    pub mode: ChargeMode,
    /// 设备时长额度，单位分钟。
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub minutes: u16,
    /// 设备电量额度，单位 mWh。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub energy_milli_wh: u64,
    /// device_power 的设备额度，等于用户实际付款金额。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub balance_cents: i64,
    /// 设备功率档位单价，单位分/小时（1–8 档）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tier_cents_per_hour: Vec<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_when_full: bool,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub max_minutes: u16,
}

impl ControlInstruction {
    /// 校验当前设备模式要求的额度字段和范围（旧版 Valid）。
    pub fn valid(&self) -> bool {
        if !self.mode.valid() || self.mode.executor() != Some(ChargeExecutor::Device) {
            return false;
        }
        match self.mode.0.as_str() {
            ChargeMode::DEVICE_DURATION => {
                // 时长模式只允许 1–4320 分钟，不允许余额或功率档位。
                (1..=crate::limits::MAX_CHARGE_MINUTES).contains(&self.minutes)
                    && self.balance_cents == 0
                    && self.tier_cents_per_hour.is_empty()
            }
            ChargeMode::DEVICE_ENERGY => {
                self.energy_milli_wh > 0
                    && self.balance_cents == 0
                    && self.tier_cents_per_hour.is_empty()
            }
            ChargeMode::DEVICE_POWER => {
                self.balance_cents > 0
                    && !self.tier_cents_per_hour.is_empty()
                    && self.tier_cents_per_hour.len() <= 8
            }
            _ => false,
        }
    }
}

/// 设备执行结果记录；reported 为 false 表示推算值（旧版 SessionActual）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionActual {
    #[serde(default)]
    pub stop_reason: StopReason,
    pub used_seconds: u32,
    pub used_milli_wh: u64,
    pub peak_watts: u32,
    /// device_power 累计消费（分）；采样间隔可能产生未消费余额。
    #[serde(default)]
    pub spent_cents: i64,
    #[serde(default)]
    pub reported: bool,
}

/// 充电结算金额及执行来源（旧版 Settlement）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settlement {
    #[serde(default)]
    pub mode: ChargeMode,
    #[serde(default)]
    pub executor: String,
    pub electric_cents: i64,
    pub service_cents: i64,
    pub total_cents: i64,
    /// 设备功率计费的未消费预付款（分）；该余额退还用户。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub residual_cents: i64,
    /// 结算金额为推算值而非设备上报值时为真。
    #[serde(default)]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stop_reason: String,
}

/// 来自一份完整方案的套餐报价（旧版 Offer）；字段与旧版 JSON 逐字节一致。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Offer {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub server_duration: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub package_id: u64,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub purchase_count: u16,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub energy_wh: u32,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub max_minutes: u16,
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub station_id: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub price_cents: i64,
    #[serde(default)]
    pub duration_minutes: u16,
}

impl Offer {
    /// 校验套餐标识、金额和额度；未填 purchase_count 按一次购买校验（旧版 Valid）。
    pub fn valid(&self) -> bool {
        let count = if self.purchase_count == 0 {
            1i64
        } else {
            self.purchase_count as i64
        };
        let mode_ok = match self.mode.as_str() {
            "amount" => self.duration_minutes == 0 && self.energy_wh == 0,
            "duration" => {
                self.duration_minutes > 0
                    && self.duration_minutes <= crate::limits::MAX_CHARGE_MINUTES
                    && self.energy_wh == 0
            }
            "energy" => {
                self.duration_minutes == 0
                    && crate::limits::ENERGY_PACKAGE_WH.contains(&self.energy_wh)
                    && self.energy_wh.is_multiple_of(1000)
            }
            _ => false,
        };
        (!self.server_duration || self.mode == "duration")
            && self.id > 0
            && self.station_id > 0
            && !self.name.is_empty()
            && self.price_cents > 0
            && self.price_cents <= MAX_RATE_CENTS * count
            && mode_ok
    }
}

/// 按购买时的 spec 和 paid 计算结算金额，不执行资金写入（旧版 SettleSession）。
/// 时长按完整分钟折算，电量按实测 Wh 折算，金额套餐最多收购买价；无效规则或计量返回错误。
pub fn settle_session(
    spec: &Spec,
    meter: &ActualMeter,
    paid: Option<&Offer>,
    actual: Option<&SessionActual>,
) -> Result<Settlement, BillingError> {
    if meter.review_required {
        return Err(BillingError::MeterReview);
    }
    let mut spec = spec.clone();
    if let (Some(scheme), Some(paid)) = (&spec.scheme, paid) {
        let p = scheme.package(paid.package_id);
        let count = if paid.purchase_count == 0 {
            1i64
        } else {
            paid.purchase_count as i64
        };
        match p {
            Some(p) if p.price_cents * count == paid.price_cents && p.mode == paid.mode => {
                if p.mode == "duration"
                    && (p.minutes as u32) * (count as u32) != paid.duration_minutes as u32
                {
                    return Err(BillingError::InvalidPricing);
                }
                spec = scheme.spec_for(&p);
            }
            _ => return Err(BillingError::InvalidPricing),
        }
    }
    validate_spec(&spec)?;
    let mut settlement = Settlement {
        mode: spec.mode.clone(),
        executor: spec
            .mode
            .executor()
            .map(|e| e.as_str().to_string())
            .unwrap_or_default(),
        ..Default::default()
    };
    if let Some(a) = actual {
        settlement.stop_reason = a.stop_reason.0.clone();
    }
    if let Some(paid) = paid
        && paid.server_duration
    {
        settlement.executor = "server".to_string();
    }
    // 固定时长套餐按购买快照的售价结算，与站点费率和功率无关。
    // 延迟停机也不超收；提前停止延续未使用时长退款规则。
    if let Some(paid) = paid
        && paid.mode == "duration"
    {
        if !paid.valid() {
            return Err(BillingError::InvalidPricing);
        }
        let mut used = meter.charged_seconds as i64;
        if !spec.mode.server_billed()
            && let Some(a) = actual
        {
            used = a.used_seconds as i64;
        }
        used /= 60;
        let limit = paid.duration_minutes as i64;
        let mut amount = paid.price_cents;
        if used < limit {
            amount = amount * used / limit;
        }
        // 套餐总价不含单独约定的电费拆分，与支付报价一致记入套餐服务费。
        settlement.service_cents = amount;
        settlement.total_cents = amount;
        settlement.estimated = actual.map(|a| !a.reported).unwrap_or(true);
        return Ok(settlement);
    }
    if let Some(paid) = paid
        && paid.mode == "energy"
    {
        if !paid.valid() {
            return Err(BillingError::InvalidPricing);
        }
        let rate = spec
            .scheme
            .as_ref()
            .and_then(|s| s.energy.clone())
            .ok_or(BillingError::InvalidPricing)?;
        let wh = meter.charged_wh as u64;
        let amount = std::cmp::min(
            (wh as i64) * (rate.electric_cents + rate.service_cents) / 1000,
            paid.price_cents,
        );
        let electric = amount * rate.electric_cents / (rate.electric_cents + rate.service_cents);
        settlement.electric_cents = electric;
        settlement.service_cents = amount - electric;
        settlement.total_cents = amount;
        return Ok(settlement);
    }
    if spec.mode.server_billed() {
        let rule = Rule {
            id: 1,
            version: 1,
            spec: spec.clone(),
            ..Default::default()
        };
        let fee = price_actual(&rule, meter)?;
        // 金额套餐使用传入的冻结费率，费用最多为购买金额。
        let mut total = fee.total_cents;
        if let Some(paid) = paid {
            if !paid.valid() {
                return Err(BillingError::InvalidPricing);
            }
            if paid.mode == "amount" && total > paid.price_cents {
                total = paid.price_cents;
            }
        }
        if total < 0 {
            return Err(BillingError::InvalidPricing);
        }
        let (electric, service) = if total != fee.total_cents {
            // 封顶或折算改变总额后，按比例重算电费与服务费，使分项合计等于总额。
            split_fee(&fee, total)
        } else {
            (fee.electric_cents, fee.service_cents)
        };
        settlement.electric_cents = electric;
        settlement.service_cents = service;
        settlement.total_cents = total;
        return Ok(settlement);
    }
    // 设备计费金额等于实际付款金额。
    let paid = paid.ok_or(BillingError::NoPaidAmount)?;
    if !paid.valid() {
        return Err(BillingError::NoPaidAmount);
    }
    let amount = paid.price_cents;
    settlement.total_cents = amount;
    settlement.estimated = actual.map(|a| !a.reported).unwrap_or(true);
    if let Some(a) = actual
        && spec.mode == ChargeMode::new(ChargeMode::DEVICE_POWER)
        && a.spent_cents > 0
    {
        settlement.residual_cents = amount - a.spent_cents;
        if settlement.residual_cents < 0 {
            return Err(BillingError::InvalidPricing);
        }
    }
    Ok(settlement)
}

/// 把一个已计价的结果按新的总额重新分摊，并保证两项之和正好等于它（旧版 splitFee）。
pub fn split_fee(fee: &crate::charging::cost::Fee, total: i64) -> (i64, i64) {
    if fee.total_cents <= 0 {
        return (0, total);
    }
    let mut electric = (fee.electric_cents * total / fee.total_cents).max(0);
    if electric > total {
        electric = total;
    }
    (electric, total - electric)
}

/// 根据生效规则和当前消费判断服务端计费是否达到上限（旧版 DecideStop）。
pub fn decide_stop(
    spec: &Spec,
    usage: &Usage,
    elapsed: Duration,
) -> Result<StopPlan, BillingError> {
    validate_spec(spec)?;
    if !spec.mode.server_billed() {
        // 设备计费由设备自身额度控制结束，不使用平台费用上限停机。
        return Ok(StopPlan {
            reason: "设备计费由设备到量自行停止".to_string(),
            ..Default::default()
        });
    }
    if let Some(tc) = &spec.time_charge
        && tc.max_minutes > 0
        && elapsed >= Duration::minutes(tc.max_minutes as i64)
    {
        return Ok(StopPlan {
            should_stop: true,
            reason: "达到最长时长".to_string(),
            ..Default::default()
        });
    }
    let fee = cost(spec, usage)?;
    let mut plan = StopPlan {
        accrued_cents: fee.total_cents,
        ..Default::default()
    };
    if spec.spend_cap_cents > 0 && fee.total_cents >= spec.spend_cap_cents {
        plan.should_stop = true;
        plan.reason = "已达到消费封顶".to_string();
        plan.grace_seconds = spec.stop_grace_seconds;
    } else if spec.spend_cap_cents == 0 {
        plan.reason = "未设置消费封顶，由下发的时间/电量到量停止".to_string();
    }
    Ok(plan)
}

/// 将套餐实收及计量适配为 SettleSession 输入，供充电和计费流程共用（旧版 PriceOfferActual）。
pub fn price_offer_actual(
    rule: &Rule,
    offer: &Offer,
    meter: &ActualMeter,
) -> Result<crate::charging::cost::Fee, BillingError> {
    let settlement = settle_session(&rule.spec, meter, Some(offer), None)?;
    Ok(crate::charging::cost::Fee {
        electric_cents: settlement.electric_cents,
        service_cents: settlement.service_cents,
        total_cents: settlement.total_cents,
        ..Default::default()
    })
}

/// 保留设备上报的实际时长与电量（旧版 ActualFromMeter）。
pub fn actual_from_meter(meter: &ActualMeter) -> SessionActual {
    let mut actual = SessionActual {
        used_seconds: meter.charged_seconds,
        used_milli_wh: meter.charged_wh as u64 * 1000,
        reported: true,
        ..Default::default()
    };
    for segment in &meter.segments {
        if segment.peak_w > actual.peak_watts {
            actual.peak_watts = segment.peak_w;
        }
    }
    actual
}

/// 以费用下界判断预算停机，避免缺失功率片段导致停机规则失效（旧版 BudgetStopAtMeter）。
/// 仅当下界达到购买金额时停机；最终费用须上下界一致或通过严格计价才能返回。
pub fn budget_stop_at_meter(
    rule: &Rule,
    offer: Option<&Offer>,
    meter: &ActualMeter,
) -> Result<(StopPlan, Option<crate::charging::cost::Fee>), BillingError> {
    let offer = match offer {
        Some(o) if o.valid() && o.mode == "amount" && rule.spec.mode.server_billed() => o,
        _ => return Err(BillingError::InvalidPricing),
    };
    if let Some(scheme) = &rule.spec.scheme {
        match scheme.package(offer.package_id) {
            Some(p) if p.mode == offer.mode && p.price_cents == offer.price_cents => {}
            _ => return Err(BillingError::InvalidPricing),
        }
    }
    let exact;
    let fee = match price_actual(rule, meter) {
        Ok(fee) => {
            exact = true;
            fee
        }
        Err(BillingError::MeterReview) if !meter.review_required && !meter.segments.is_empty() => {
            let lower = budget_bound(rule, meter, false)?;
            match budget_bound(rule, meter, true) {
                Ok(upper) => {
                    exact = lower.electric_cents == upper.electric_cents
                        && lower.service_cents == upper.service_cents;
                }
                Err(_) => exact = false,
            }
            lower
        }
        Err(e) => return Err(e),
    };
    let mut plan = StopPlan {
        accrued_cents: fee.total_cents,
        should_stop: fee.total_cents >= offer.price_cents,
        ..Default::default()
    };
    if !plan.should_stop {
        return Ok((plan, None));
    }
    plan.reason = "budget_exhausted".to_string();
    if !exact {
        // 预算停机判断不能代替最终电费、服务费的拆分。
        return Ok((plan, None));
    }
    let (electric, service) = split_fee(&fee, offer.price_cents);
    Ok((
        plan,
        Some(crate::charging::cost::Fee {
            electric_cents: electric,
            service_cents: service,
            total_cents: electric + service,
            ..Default::default()
        }),
    ))
}

/// 复制费率和分段，用最低或最高可用价格计算费用下界或上界；不修改原始数据（旧版 budgetBound）。
fn budget_bound(
    rule: &Rule,
    meter: &ActualMeter,
    upper: bool,
) -> Result<crate::charging::cost::Fee, BillingError> {
    use crate::charging::spec::ChargeMode as CM;
    if validate_spec(&rule.spec).is_err() {
        return Err(BillingError::InvalidPricing);
    }
    let electric = match &rule.spec.electric {
        Some(e) => e.clone(),
        None => return Err(BillingError::InvalidPricing),
    };
    let is_energy = rule.spec.mode == ChargeMode::new(CM::SERVER_ENERGY);
    let is_realtime = rule.spec.mode == ChargeMode::new(CM::SERVER_REALTIME_POWER);
    let mut line = electric.clone();
    for pi in 0..line.periods.len() {
        let period = &mut line.periods[pi];
        if is_energy {
            // 缺少边界电量时，用最低或最高单价计算费用范围，不按时长分摊电量。
            let bound = |cur: i64, candidate: i64| -> i64 {
                if upper {
                    cur.max(candidate)
                } else {
                    cur.min(candidate)
                }
            };
            for candidate in &electric.periods {
                period.electric_cents = bound(period.electric_cents, candidate.electric_cents);
                period.service_cents = bound(period.service_cents, candidate.service_cents);
            }
            continue;
        }
        for ti in 0..period.tiers.len() {
            let mut first = ti;
            if upper || (is_realtime && ti == 0) {
                first = 0;
            }
            if is_realtime && !upper && ti > 0 {
                continue;
            }
            // 缺测区间可能出现更高峰值。阶梯价格不随功率递增时，仍须计算费用下界。
            let bound = |cur: i64, candidate: i64| -> i64 {
                if upper {
                    cur.max(candidate)
                } else {
                    cur.min(candidate)
                }
            };
            for candidate in electric.periods[pi].tiers[first..].iter() {
                period.tiers[ti].electric_cents =
                    bound(period.tiers[ti].electric_cents, candidate.electric_cents);
                period.tiers[ti].service_cents =
                    bound(period.tiers[ti].service_cents, candidate.service_cents);
            }
        }
    }
    let mut bounded_spec = rule.spec.clone();
    bounded_spec.electric = Some(line);
    let mut bounded_meter = meter.clone();
    for segment in bounded_meter.segments.iter_mut() {
        if segment.power_w.is_none() && (is_realtime || segment.peak_w == 0) {
            segment.power_w = Some(0);
        }
    }
    let bounded_rule = Rule {
        id: rule.id,
        station_id: rule.station_id,
        device_id: rule.device_id.clone(),
        version: rule.version,
        spec: bounded_spec,
        channel: rule.channel.clone(),
    };
    price_actual(&bounded_rule, &bounded_meter)
}

/// 报价结果；预计电量用 kWh 文本表示，金额单位为分（旧版 Estimate）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Estimate {
    pub estimated_kwh: String,
    pub estimated_minutes: u16,
    #[serde(default)]
    pub mode: ChargeMode,
    /// 设备计费模式下为空，该模式不使用服务端费率估算。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub basis: String,
    pub electric_cents: i64,
    pub service_cents: i64,
    pub total_cents: i64,
    /// 设备计费已收的预付款（分），不由服务端费率推算。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub prepaid_cents: i64,
    #[serde(default)]
    pub charge_mode: u8,
    #[serde(default)]
    pub charge_quantity: u16,
}

/// 将预计电量均匀分配到请求时长，再调用 Cost 计算报价（旧版 EstimateCharge）。
/// 报价使用均匀负载假设，最终结算以实际计量为准；设备计费直接返回预付款金额。
pub fn estimate_charge(
    rule: &Rule,
    energy: &str,
    minutes: u16,
    start: DateTime<Utc>,
    prepaid_cents: i64,
) -> Result<Estimate, BillingError> {
    if rule.id == 0
        || !rule.spec.mode.valid()
        || minutes == 0
        || minutes > crate::limits::MAX_ESTIMATE_MINUTES
        || validate_spec(&rule.spec).is_err()
        || !energy_pattern_ok(energy)
    {
        return Err(BillingError::InvalidPricing);
    }
    let kwh = parse_energy(energy)?;
    if !rule.spec.mode.server_billed() {
        if !(0..=MAX_RATE_CENTS).contains(&prepaid_cents) {
            return Err(BillingError::InvalidPricing);
        }
        return Ok(Estimate {
            estimated_kwh: fixed3(&kwh),
            estimated_minutes: minutes,
            mode: rule.spec.mode.clone(),
            total_cents: prepaid_cents,
            prepaid_cents,
            charge_quantity: minutes,
            ..Default::default()
        });
    }
    let wh = int_part(kwh * Decimal::from(1000));
    if !(1..=crate::limits::MAX_ESTIMATE_ENERGY_WH).contains(&wh) {
        return Err(BillingError::InvalidPricing);
    }
    let mut usage = Usage {
        start: Some(start),
        end: Some(start + Duration::minutes(minutes as i64)),
        energy_wh: wh as u64,
        samples: Vec::new(),
        channel: None,
    };
    let base = wh / minutes as i64;
    let remainder = wh % minutes as i64;
    for i in 0..minutes as i64 {
        let mut energy = base;
        if i < remainder {
            energy += 1;
        }
        usage.samples.push(crate::charging::spec::Sample {
            start: start + Duration::minutes(i),
            end: start + Duration::minutes(i + 1),
            energy_wh: energy as u64,
            power_w: 0,
            power_known: false,
        });
    }
    let fee = match cost(&rule.spec, &usage) {
        Ok(f) => f,
        Err(_) => return Err(BillingError::InvalidPricing),
    };
    if fee.total_cents <= 0 {
        return Err(BillingError::InvalidPricing);
    }
    Ok(Estimate {
        estimated_kwh: fixed3(&kwh),
        estimated_minutes: minutes,
        mode: rule.spec.mode.clone(),
        basis: fee.basis.clone(),
        electric_cents: fee.electric_cents,
        service_cents: fee.service_cents,
        total_cents: fee.total_cents,
        charge_mode: 0,
        charge_quantity: minutes,
        ..Default::default()
    })
}

/// shopspring StringFixed(3)：half away from zero 舍入到 3 位小数固定输出。
fn fixed3(value: &Decimal) -> String {
    use rust_decimal::RoundingStrategy;
    let rounded = value.round_dp_with_strategy(3, RoundingStrategy::MidpointAwayFromZero);
    format!("{:.3}", rounded)
}

/// 单次计费最大切片数引用（保持与旧版常量清单一致；MAX_SLICES 供 usage_from_meter 使用）。
#[allow(dead_code)]
pub fn max_slices() -> usize {
    MAX_SLICES
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// Decimal 除以 1000 的 16 位精度版（估算路径 Wh→kWh 不需要，占位保证 div_prec16 被引用）。
#[allow(dead_code)]
fn _ref_div(a: Decimal) -> Decimal {
    div_prec16(a, Decimal::from(1))
}
