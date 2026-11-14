//! 计价配置（Spec）：模式枚举、费率时段、全天校验与用量输入。
//! 语义基准：旧仓库 internal/domain/charging/{spec,mode,cost}.go，逐条对齐；
//! 定点十进制用 rust_decimal，除法/取整模拟 shopspring 语义（Div=16 位小数、Round=远离零）。

use chrono::{DateTime, FixedOffset, Timelike, Utc};
use rust_decimal::prelude::*;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

/// 北京时区（固定 +8，旧版 time.FixedZone）。
pub fn beijing() -> FixedOffset {
    match FixedOffset::east_opt(8 * 3600) {
        Some(o) => o,
        None => panic!("北京时区偏移固定为 +8"),
    }
}

/// 单价上限（旧版 maxRateCents）：所有单价与封顶的合法上界。
pub const MAX_RATE_CENTS: i64 = 1_000_000;

/// 单次计费的最大切片数（7 天 × 每分钟），防止无界计算（旧版 maxSlices）。
pub const MAX_SLICES: usize = 7 * 24 * 60;

/// 计费域错误：变体与旧版哨兵错误一一对应。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BillingError {
    /// ErrInvalidPricing：规则、价格或用量参数不符合计算要求。
    #[error("invalid quote input or pricing rule")]
    InvalidPricing,
    /// ErrMeterReview：读数缺失、分段不连续或合计不符，须人工核对计量。
    #[error("实际计量不足或矛盾，需补充分时计量后核算")]
    MeterReview,
    /// ErrNotServerBilled：当前模式由设备计费，服务端不计算费用。
    #[error("该计费方式由设备执行，服务端不计算费用")]
    NotServerBilled,
    /// ErrNoPaidAmount：设备计费缺少实付金额。
    #[error("设备计费缺少实付金额，无法结算")]
    NoPaidAmount,
}

// ---------- 数值工具（shopspring 语义模拟） ----------

/// 模拟 shopspring `Div`：商保留 16 位小数，half away from zero。
pub fn div_prec16(a: Decimal, b: Decimal) -> Decimal {
    (a / b).round_dp_with_strategy(16, RoundingStrategy::MidpointAwayFromZero)
}

/// 模拟 shopspring `Round(0)`：取整到分，half away from zero；溢出 i64 返回 None。
pub fn round_to_cents(value: Decimal) -> Option<i64> {
    let rounded = value.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
    rounded.to_i64()
}

/// 模拟 shopspring `StringFixed(6)`：先按 half away from zero 舍入到 6 位再固定输出。
pub fn string_fixed6(value: Decimal) -> String {
    let rounded = value.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero);
    format!("{:.6}", rounded)
}

/// 模拟 shopspring `IntPart()`：截断取整数部分。
pub fn int_part(value: Decimal) -> i64 {
    value.trunc().to_i64().unwrap_or(0)
}

/// decimal(1, -3)＝0.001 的字面量（旧版 parseEnergy 下界）。
fn decimal_milli() -> Decimal {
    Decimal::new(1, 3)
}

// ---------- 计费模式枚举（旧版 mode.go） ----------

/// 计费执行方：server（平台计价）或 device（设备执行额度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeExecutor {
    Server,
    Device,
}

impl ChargeExecutor {
    pub fn is_device(self) -> bool {
        self == ChargeExecutor::Device
    }
    /// JSON/DB 字面量（旧版字符串值）。
    pub fn as_str(self) -> &'static str {
        match self {
            ChargeExecutor::Server => "server",
            ChargeExecutor::Device => "device",
        }
    }
}

/// 计费模式：服务端三种 + 设备三种；字符串字面量与旧版逐字节一致。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChargeMode(pub String);

impl ChargeMode {
    pub const SERVER_REALTIME_POWER: &'static str = "server_realtime_power";
    pub const SERVER_MAX_POWER: &'static str = "server_max_power";
    pub const SERVER_ENERGY: &'static str = "server_energy";
    pub const DEVICE_DURATION: &'static str = "device_duration";
    pub const DEVICE_ENERGY: &'static str = "device_energy";
    pub const DEVICE_POWER: &'static str = "device_power";

    pub fn new(s: impl Into<String>) -> Self {
        ChargeMode(s.into())
    }

    /// 执行方；未知模式返回 None（旧版返回空串）。
    pub fn executor(&self) -> Option<ChargeExecutor> {
        match self.0.as_str() {
            Self::SERVER_REALTIME_POWER | Self::SERVER_MAX_POWER | Self::SERVER_ENERGY => {
                Some(ChargeExecutor::Server)
            }
            Self::DEVICE_DURATION | Self::DEVICE_ENERGY | Self::DEVICE_POWER => {
                Some(ChargeExecutor::Device)
            }
            _ => None,
        }
    }

    /// 该模式是否由服务端 Cost 计算最终费用。
    pub fn server_billed(&self) -> bool {
        self.executor() == Some(ChargeExecutor::Server)
    }

    /// 当前版本是否支持该计费模式。
    pub fn valid(&self) -> bool {
        self.executor().is_some()
    }

    /// 服务端模式对应的电价算法；非服务端模式返回 None（旧版空串）。
    pub fn basis_for(&self) -> Option<ServerBasis> {
        match self.0.as_str() {
            Self::SERVER_REALTIME_POWER => Some(ServerBasis::RealtimePower),
            Self::SERVER_MAX_POWER => Some(ServerBasis::MaxPower),
            Self::SERVER_ENERGY => Some(ServerBasis::Energy),
            _ => None,
        }
    }
}

/// 服务端电费算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerBasis {
    /// 按各分段功率选档，以小时单价计算费用。
    RealtimePower,
    /// 按每个费率时段的峰值功率选择该时段档位。
    MaxPower,
    /// 按各时段的电量与每 kWh 单价计费。
    Energy,
}

impl ServerBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerBasis::RealtimePower => "realtime_power",
            ServerBasis::MaxPower => "max_power",
            ServerBasis::Energy => "energy",
        }
    }
}

/// 旧版独立服务费算法（当前 Cost 不使用，保留取值词表用于校验历史数据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceBasis(pub String);

impl ServiceBasis {
    pub fn valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            "none" | "energy" | "minute_power" | "minute" | "session"
        )
    }
}

/// 旧版阶梯价格单位标记（新模板禁止，保留词表）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierPriceBasis(pub String);

/// 充电入口渠道（当前 Cost 不按渠道调整费用，保留词表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    #[default]
    Default,
    Temp,
    Card,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Default => "default",
            Channel::Temp => "temp",
            Channel::Card => "card",
        }
    }
}

// ---------- 费率配置结构 ----------

/// 功率阶梯档：仅保存上界（含端点），下界由上一档上界加 1 推导。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tier {
    pub max_watts: i64,
    pub electric_cents: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub service_cents: i64,
}

/// 一天内一个费率时段：开始分钟取上一时段结束，第一段从零点起。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Period {
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub service_cents: i64,
    /// 从零点起的结束分钟（1..1440），严格递增，最后一项必须为 1440。
    pub end_minute: i64,
    /// 电量计费的单一电价（分/kWh）；该计价方式不使用阶梯。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub electric_cents: i64,
    /// 该时段适用的功率阶梯；电量计价方式下为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<Tier>,
}

/// 服务端电价配置：算法 + 全天时段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ElectricLine {
    pub basis: String,
    pub periods: Vec<Period>,
}

/// 旧版独立服务费配置（当前 Cost 不使用，保留结构用于校验历史数据）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceLine {
    pub basis: ServiceBasis,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cents_per_kwh: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cents_per_minute: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub cents_per_session: i64,
}

/// 旧版电费渠道倍率（当前 Cost 不使用，保留结构）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMultiplier {
    #[serde(default)]
    pub temp_bp: i32,
    #[serde(default)]
    pub card_bp: i32,
}

/// 按时长充电的停止条件（不定义费率）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TimeCharge {
    #[serde(default)]
    pub stop_when_full: bool,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub max_minutes: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub float_power_deci_watts: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub float_seconds: u16,
}

/// 完整计价配置：预估与结算共用此结构和 Cost 引擎。
/// serde 字段名与旧版 spec JSON 逐字节一致（omitempty ↔ skip_serializing_if）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<super::charging_scheme::Scheme>,
    pub mode: ChargeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub electric: Option<ElectricLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<ChannelMultiplier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_price_basis: Option<TierPriceBasis>,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub loss_rate_bp: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub free_minutes: i32,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub min_electric_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_charge: Option<TimeCharge>,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub spend_cap_cents: i64,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub stop_grace_seconds: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_charge_way: String,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub card_max_minutes: u16,
    #[serde(default)]
    pub display: super::charging_scheme::Display,
}

/// 连续区间的用量样本：功率计费由 Cost 按时段切分，电量计费要求区间内费率不变。
#[derive(Debug, Clone)]
pub struct Sample {
    pub power_known: bool,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub energy_wh: u64,
    /// 区间功率（W）；为 0 且 power_known=false 时不能用于功率计费。
    pub power_w: u32,
}

/// Cost 的用量输入：时间跨度最多 7 天，电量单位 Wh。
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub energy_wh: u64,
    pub samples: Vec<Sample>,
    pub channel: Option<Channel>,
}

impl Usage {
    pub fn minutes(&self) -> i64 {
        match (self.start, self.end) {
            (Some(s), Some(e)) if e > s => (e - s).num_minutes(),
            _ => 0,
        }
    }
}

// ---------- 校验与全天展开 ----------

/// 全天时段展开为按分钟索引的费率表；结束分钟严格递增并覆盖 [0,1440)。
/// 校验失败返回 None（旧版返回零值表 + 错误；调用方校验在先，不会触达）。
pub fn compile_periods(periods: &[Period]) -> Option<Box<[Period]>> {
    if periods.is_empty() || periods.len() > 48 {
        return None;
    }
    let mut schedule: Vec<Period> = vec![Period::default(); 1440];
    let mut cursor = 0i64;
    for period in periods {
        if period.end_minute <= cursor || period.end_minute > 1440 {
            return None;
        }
        for minute in cursor..period.end_minute {
            schedule[minute as usize] = period.clone();
        }
        cursor = period.end_minute;
    }
    if cursor != 1440 {
        return None;
    }
    Some(schedule.into_boxed_slice())
}

/// 功率阶梯连续覆盖校验；电量单价与功率阶梯不能同时配置（旧版 validateTiers）。
fn validate_tiers(basis_is_energy: bool, period: &Period) -> Result<(), BillingError> {
    if !(0..=MAX_RATE_CENTS).contains(&period.service_cents) {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=MAX_RATE_CENTS).contains(&period.electric_cents) {
        return Err(BillingError::InvalidPricing);
    }
    if basis_is_energy {
        if !period.tiers.is_empty() {
            return Err(BillingError::InvalidPricing);
        }
        return Ok(());
    }
    if period.electric_cents != 0 || period.tiers.is_empty() || period.tiers.len() > 8 {
        return Err(BillingError::InvalidPricing);
    }
    let mut previous = -1i64;
    for tier in &period.tiers {
        if tier.max_watts < 0 || tier.max_watts <= previous || tier.max_watts > 9990 {
            return Err(BillingError::InvalidPricing);
        }
        if !(0..=MAX_RATE_CENTS).contains(&tier.electric_cents)
            || !(0..=MAX_RATE_CENTS).contains(&tier.service_cents)
        {
            return Err(BillingError::InvalidPricing);
        }
        previous = tier.max_watts;
    }
    Ok(())
}

/// 发布和计费共用的规则校验：确保已发布规则满足结算执行条件（旧版 ValidateSpec）。
pub fn validate_spec(spec: &Spec) -> Result<(), BillingError> {
    if !spec.mode.valid() {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=1440).contains(&spec.free_minutes) {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=MAX_RATE_CENTS).contains(&spec.min_electric_cents) {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=MAX_RATE_CENTS).contains(&spec.spend_cap_cents) {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=crate::limits::MAX_STOP_GRACE_SECONDS).contains(&spec.stop_grace_seconds) {
        return Err(BillingError::InvalidPricing);
    }
    if !(0..=crate::limits::MAX_LOSS_RATE_BP).contains(&spec.loss_rate_bp) {
        return Err(BillingError::InvalidPricing);
    }
    if let Some(m) = &spec.multiplier {
        for bp in [m.temp_bp, m.card_bp] {
            if !(0..=crate::limits::MAX_LOSS_RATE_BP).contains(&bp) {
                return Err(BillingError::InvalidPricing);
            }
        }
    }
    if spec.card_max_minutes > crate::limits::MAX_CHARGE_MINUTES {
        // 刷卡时长使用 uint16 分钟字段，超出范围时拒绝，不截断数值。
        return Err(BillingError::InvalidPricing);
    }
    if let Some(tc) = &spec.time_charge
        && (tc.max_minutes > crate::limits::MAX_CHARGE_MINUTES
            || tc.float_seconds > crate::limits::MAX_FLOAT_SECONDS
            || tc.float_power_deci_watts > crate::limits::MAX_FLOAT_POWER_DECI_WATTS)
    {
        return Err(BillingError::InvalidPricing);
    }
    if let Some(service) = &spec.service {
        if !service.basis.valid() {
            return Err(BillingError::InvalidPricing);
        }
        for amount in [
            service.cents_per_kwh,
            service.cents_per_minute,
            service.cents_per_session,
        ] {
            if !(0..=MAX_RATE_CENTS).contains(&amount) {
                return Err(BillingError::InvalidPricing);
            }
        }
    }
    if !spec.mode.server_billed() {
        // 设备计费不使用服务端费率，拒绝携带无效费率的规则。
        if spec.electric.is_some() || spec.service.is_some() || spec.multiplier.is_some() {
            return Err(BillingError::InvalidPricing);
        }
        return Ok(());
    }
    let electric = match &spec.electric {
        Some(e) => e,
        None => return Err(BillingError::InvalidPricing),
    };
    let want_basis = spec
        .mode
        .basis_for()
        .ok_or(BillingError::InvalidPricing)?
        .as_str();
    if electric.basis != want_basis {
        // 计费模式必须与电价算法一致。
        return Err(BillingError::InvalidPricing);
    }
    for period in &electric.periods {
        validate_tiers(want_basis == ServerBasis::Energy.as_str(), period)?;
    }
    compile_periods(&electric.periods).ok_or(BillingError::InvalidPricing)?;
    Ok(())
}

/// 新写入或新发布的模板校验（结算仍用 validate_spec）；
/// 避免把旧订单的冻结价格拒掉或按新算法重算（旧版 ValidateTemplateSpec）。
pub fn validate_template_spec(spec: &Spec) -> Result<(), BillingError> {
    if let Some(scheme) = &spec.scheme {
        return scheme.validate().map_err(|_| BillingError::InvalidPricing);
    }
    validate_spec(spec)?;
    if spec.service.is_some()
        || spec.spend_cap_cents != 0
        || spec.tier_price_basis.is_some()
        || spec.loss_rate_bp != 0
        || spec.multiplier.is_some()
    {
        return Err(BillingError::InvalidPricing);
    }
    Ok(())
}

/// 判断两个时段费率是否完全一致（单价与阶梯逐项比较）。
/// 仅比较阶梯会误将不同的电量电价视为相同，导致跨费率电量错误分摊。
fn same_rate(a: &Period, b: &Period) -> bool {
    if a.electric_cents != b.electric_cents
        || a.service_cents != b.service_cents
        || a.tiers.len() != b.tiers.len()
    {
        return false;
    }
    a.tiers.iter().zip(b.tiers.iter()).all(|(x, y)| x == y)
}

/// 比较区间内的费率是否不变；电量/实时功率费率相同时可直接使用区间总电量。
/// 其他模式（含最大功率）直接返回 true（旧版行为）。
pub fn spec_is_uniform_over(spec: &Spec, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let basis_is_relevant = match &spec.electric {
        None => return true,
        Some(e) => {
            e.basis != ServerBasis::Energy.as_str()
                && e.basis != ServerBasis::RealtimePower.as_str()
        }
    };
    if basis_is_relevant {
        return true;
    }
    let Some(electric) = spec.electric.as_ref() else {
        return true;
    };
    let Some(schedule) = compile_periods(&electric.periods) else {
        return false;
    };
    let bj = beijing();
    let mut first: Option<&Period> = None;
    let mut at = truncate_minute(start.with_timezone(&bj));
    let end_min = truncate_minute(end.with_timezone(&bj));
    loop {
        if at > end_min {
            break;
        }
        let idx = (at.hour() * 60 + at.minute()) as usize;
        let period = &schedule[idx];
        match first {
            None => first = Some(period),
            Some(f) if !same_rate(f, period) => return false,
            _ => {}
        }
        at += chrono::Duration::minutes(1);
    }
    true
}

/// Go time.Truncate(time.Minute)：去掉秒与亚秒，保留分钟对齐（在指定时区视角下）。
pub fn truncate_minute(t: chrono::DateTime<FixedOffset>) -> chrono::DateTime<FixedOffset> {
    t.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t)
}

/// 用户输入电量解析：范围 0.001–100 kWh（旧版 parseEnergy）。
pub fn parse_energy(value: &str) -> Result<Decimal, BillingError> {
    let kwh: Decimal = value.parse().map_err(|_| BillingError::InvalidPricing)?;
    if kwh < decimal_milli() || kwh > Decimal::from(100) {
        return Err(BillingError::InvalidPricing);
    }
    Ok(kwh)
}

/// 用户输入电量的格式白名单（旧版 energyPattern）。
pub fn energy_pattern_ok(value: &str) -> bool {
    // ^[0-9]{1,3}(\.[0-9]{1,3})?$
    let bytes = value.as_bytes();
    let (int_part, frac_part) = match bytes.iter().position(|&b| b == b'.') {
        Some(dot) => (&bytes[..dot], Some(&bytes[dot + 1..])),
        None => (bytes, None),
    };
    if int_part.is_empty() || int_part.len() > 3 || !int_part.iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match frac_part {
        None => true,
        Some(f) => !f.is_empty() && f.len() <= 3 && f.iter().all(|b| b.is_ascii_digit()),
    }
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}
