//! 完整计费方案（Scheme）：模板、发布副本与订单快照使用同一结构；
//! 校验、套餐查找、计价配置转换、报价与费用预览。
//! 语义基准：旧仓库 internal/domain/charging/scheme.go，逐条对齐（错误文案与旧版一致）。

use crate::charging_actual::{ActualMeter, Rule};
use crate::charging_cost::{Fee, cutoff_meter};
use crate::charging_settle::{Offer, Settlement, actual_from_meter, settle_session};
/// 引擎计价配置：与本模块的存储包装 Spec（SpecSnapshot 别名）区分。
use crate::charging_spec::Spec as EngineSpec;
use crate::charging_spec::{
    BillingError, ChargeMode, TimeCharge, validate_spec as engine_validate_spec,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 单价上限（旧版 maxRateCents），校验共享。
pub const MAX_RATE_CENTS: i64 = 1_000_000;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SchemeError(pub String);

// ---------- 结构定义（字段与旧版 JSON tag 逐字节一致） ----------

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// 完整计费方案。设备覆盖必须替换整份方案，不能合并局部字段。
/// 容器级 serde(default)：历史模板可能缺字段，反序列化时逐字段补默认。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Scheme {
    pub name: String,
    pub remark: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<AmountMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<EnergyMode>,
    pub packages: Vec<Package>,
    pub policy: AmountPolicy,
    pub stop: TimeCharge,
    pub card: CardPolicy,
    pub display: Display,
}

/// 金额套餐的计费算法与全天费率时段（旧版 AmountMode）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmountMode {
    pub algorithm: ChargeMode,
    #[serde(default)]
    pub periods: Vec<crate::charging_spec::Period>,
}

/// 电量套餐固定单价，单位分/kWh（旧版 EnergyMode）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnergyMode {
    pub electric_cents: i64,
    pub service_cents: i64,
}

/// 方案中的购买档位：价格单位分，额度为分钟或整数 kWh（旧版 Package）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Package {
    pub id: u64,
    pub name: String,
    /// 金额、时长、电量：amount / duration / energy（旧版字面量）。
    pub mode: String,
    pub price_cents: i64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub minutes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub kwh: u64,
}

/// 金额套餐的免单时长、最低电费和最长时长（旧版 AmountPolicy）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AmountPolicy {
    pub free_minutes: i32,
    pub min_electric_cents: i64,
    pub max_minutes: u16,
    pub loss_rate_bp: i32,
    pub channel_bp: i32,
}

/// 刷卡购买的时长套餐和累计分钟上限（旧版 CardPolicy）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CardPolicy {
    pub package_id: u64,
    pub max_minutes: u16,
}

/// 用户端呈现配置，与实际计费参数独立（旧版 Display）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Display {
    #[serde(default)]
    pub duration: Vec<DisplayLine>,
    #[serde(default)]
    pub energy: Vec<DisplayLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayLine {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// spec_json 的存储包装：新版只读写 `scheme`，历史遗留字段经 `extra` 原样往返不丢失。
/// 计费引擎使用 [`crate::charging_spec::Spec`]（计价配置），两者由 [`Scheme::spec_for`] 衔接。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpecSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<Scheme>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// 兼容别名：阶段 03 期间存储包装曾命名为 Spec，定价模块按此路径引用。
pub type Spec = SpecSnapshot;

// ---------- Scheme 方法 ----------

impl Scheme {
    /// 补齐默认值：最长时长 600 分钟、渠道倍率 10000（旧版 Normalized）。
    pub fn normalized(mut self) -> Self {
        if self.policy.max_minutes == 0 {
            self.policy.max_minutes = 600;
        }
        if self.card.max_minutes == 0 {
            self.card.max_minutes = 600;
        }
        if self.policy.channel_bp == 0 {
            self.policy.channel_bp = 10000;
        }
        self
    }

    /// 全量校验：先补默认值，不修改调用方方案（旧版 Validate；文案逐字节一致）。
    pub fn validate(&self) -> Result<(), SchemeError> {
        let s = self.clone().normalized();
        let name_chars = s.name.chars().count();
        if name_chars == 0 || name_chars > 64 || s.remark.chars().count() > 255 {
            return Err(SchemeError("请填写方案名称（最多64字）及有效说明".into()));
        }
        if s.packages.is_empty() || s.packages.len() > 99 {
            return Err(SchemeError("请配置1～99个支付套餐".into()));
        }
        if !(60..=4320).contains(&s.policy.max_minutes)
            || !(60..=4320).contains(&s.card.max_minutes)
        {
            return Err(SchemeError("金额与刷卡时长上限须为1～72小时".into()));
        }
        if s.policy.free_minutes < 0
            || s.policy.free_minutes > s.policy.max_minutes as i32
            || s.policy.min_electric_cents < 0
            || s.policy.min_electric_cents > MAX_RATE_CENTS
        {
            return Err(SchemeError("免费时长或最低电费无效".into()));
        }
        // 当前方案只接受零损耗和 1 倍渠道系数。
        if s.policy.loss_rate_bp != 0 || s.policy.channel_bp != 10000 {
            return Err(SchemeError(
                "损耗与渠道系数组合规则尚未确认，当前请关闭损耗并使用1倍系数".into(),
            ));
        }
        if let Some(amount) = &s.amount {
            if !amount.algorithm.server_billed() {
                return Err(SchemeError(
                    "金额模式须选择最大功率、实时功率或电量算法".into(),
                ));
            }
            let spec = EngineSpec {
                mode: amount.algorithm.clone(),
                electric: Some(crate::charging_spec::ElectricLine {
                    basis: amount
                        .algorithm
                        .basis_for()
                        .map(|b| b.as_str().to_string())
                        .unwrap_or_default(),
                    periods: amount.periods.clone(),
                }),
                ..Default::default()
            };
            if engine_validate_spec(&spec).is_err() {
                return Err(SchemeError(
                    "请完整填写全天时段及电费、服务费，功率档位须递增".into(),
                ));
            }
        }
        if let Some(energy) = &s.energy
            && (energy.electric_cents < 0
                || energy.service_cents < 0
                || energy.electric_cents + energy.service_cents <= 0
                || energy.electric_cents > MAX_RATE_CENTS
                || energy.service_cents > MAX_RATE_CENTS)
        {
            return Err(SchemeError("请填写设备电量的固定电费与服务费单价".into()));
        }
        let mut ids = std::collections::HashSet::new();
        let (mut amount, mut energy, mut card) = (false, false, s.card.package_id == 0);
        for p in &s.packages {
            if p.id == 0
                || p.id > 99
                || ids.contains(&p.id)
                || p.name.is_empty()
                || p.name.chars().count() > 64
            {
                return Err(SchemeError("套餐编号须唯一（1～99），名称不能为空".into()));
            }
            ids.insert(p.id);
            match p.mode.as_str() {
                "amount" => {
                    amount = true;
                    if s.amount.is_none()
                        || p.minutes != 0
                        || p.kwh != 0
                        || p.price_cents <= 0
                        || p.price_cents < s.policy.min_electric_cents
                        || p.price_cents > MAX_RATE_CENTS
                    {
                        return Err(SchemeError(
                            "金额套餐须绑定金额算法，价格不能低于最低电费".into(),
                        ));
                    }
                }
                "duration" => {
                    if p.minutes == 0
                        || p.minutes > 4320
                        || p.kwh != 0
                        || p.price_cents <= 0
                        || p.price_cents > MAX_RATE_CENTS
                    {
                        return Err(SchemeError("时长套餐须填写价格与1～4320分钟".into()));
                    }
                    if p.id == s.card.package_id {
                        card = true;
                        if p.minutes > s.card.max_minutes as u64 {
                            return Err(SchemeError("刷卡套餐不能超过累计上限".into()));
                        }
                    }
                }
                "energy" => {
                    energy = true;
                    let e = s.energy.as_ref().ok_or_else(|| {
                        SchemeError("电量套餐须为1～65整数度并配置固定单价".into())
                    })?;
                    if !(1..=65).contains(&p.kwh) || p.minutes != 0 {
                        return Err(SchemeError("电量套餐须为1～65整数度并配置固定单价".into()));
                    }
                    if p.price_cents != p.kwh as i64 * (e.electric_cents + e.service_cents)
                        || p.price_cents > MAX_RATE_CENTS
                    {
                        return Err(SchemeError(
                            "电量套餐价格必须由度数及电费、服务费自动计算".into(),
                        ));
                    }
                }
                _ => return Err(SchemeError("套餐仅支持金额、时长、电量".into())),
            }
        }
        if (s.amount.is_some() && !amount) || (s.energy.is_some() && !energy) {
            return Err(SchemeError("已启用的模式必须配置支付套餐".into()));
        }
        if !card {
            return Err(SchemeError("刷卡须明确选择一个已有时长套餐".into()));
        }
        Ok(())
    }

    /// 按 id 查找方案内套餐；None 表示未找到（旧版 Package）。
    pub fn package(&self, id: u64) -> Option<Package> {
        self.packages.iter().find(|p| p.id == id).cloned()
    }

    /// 将冻结方案与所选套餐转换为计价配置（旧版 SpecFor，返回计费引擎 Spec）。
    pub fn spec_for(&self, p: &Package) -> EngineSpec {
        let s = self.clone().normalized();
        let mut spec = EngineSpec {
            scheme: Some(s.clone()),
            display: s.display.clone(),
            time_charge: Some(s.stop.clone()),
            card_max_minutes: s.card.max_minutes,
            ..Default::default()
        };
        match p.mode.as_str() {
            "amount" => {
                if let Some(amount) = &s.amount {
                    spec.mode = amount.algorithm.clone();
                    spec.electric = Some(crate::charging_spec::ElectricLine {
                        basis: amount
                            .algorithm
                            .basis_for()
                            .map(|b| b.as_str().to_string())
                            .unwrap_or_default(),
                        periods: amount.periods.clone(),
                    });
                }
                spec.free_minutes = s.policy.free_minutes;
                spec.min_electric_cents = s.policy.min_electric_cents;
                spec.time_charge = Some(TimeCharge {
                    stop_when_full: s.stop.stop_when_full,
                    max_minutes: s.policy.max_minutes,
                    ..Default::default()
                });
            }
            "duration" => spec.mode = ChargeMode::new(ChargeMode::DEVICE_DURATION),
            "energy" => spec.mode = ChargeMode::new(ChargeMode::DEVICE_ENERGY),
            _ => {}
        }
        spec
    }

    /// 为各套餐生成报价，ID 由规则 ID 和套餐 ID 组成；电量由 kWh 换算为 Wh（旧版 Offers）。
    pub fn offers(&self, rule: &Rule) -> Vec<Offer> {
        let n = self.clone().normalized();
        self.packages
            .iter()
            .map(|p| Offer {
                id: rule.id * 100 + p.id,
                package_id: p.id,
                station_id: rule.station_id,
                device_id: rule.device_id.clone(),
                name: p.name.clone(),
                mode: p.mode.clone(),
                price_cents: p.price_cents,
                duration_minutes: p.minutes as u16,
                energy_wh: (p.kwh * 1000) as u32,
                max_minutes: n.policy.max_minutes,
                ..Default::default()
            })
            .collect()
    }
}

// ---------- 费用预览（旧版 Preview） ----------

/// 费用预览、退款差额和模拟操作结果；金额单位分，不表示已扣款或退款（旧版 Preview）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Preview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_fee: Option<Fee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_fee: Option<Fee>,
    #[serde(default)]
    pub package: Package,
    #[serde(default)]
    pub paid_cents: i64,
    #[serde(default)]
    pub settlement: Settlement,
    #[serde(default)]
    pub refund_cents: i64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub reason: String,
}

impl Scheme {
    /// 计算一次购买的费用与退款；计量不足返回 meter_review 状态，不写入数据（旧版 Preview）。
    pub fn preview(&self, id: u64, meter: ActualMeter) -> Result<Preview, SchemeError> {
        self.preview_purchases(id, meter, 1)
    }

    fn preview_purchases(
        &self,
        id: u64,
        mut meter: ActualMeter,
        count: u16,
    ) -> Result<Preview, SchemeError> {
        self.validate()?;
        let p = self
            .package(id)
            .ok_or_else(|| SchemeError("请选择方案内套餐".into()))?;
        let n = self.clone().normalized();
        if count < 1
            || (count > 1
                && (p.mode != "duration"
                    || n.card.package_id != id
                    || (p.minutes as u32) * (count as u32) > n.card.max_minutes as u32))
        {
            return Err(SchemeError("刷卡套餐或累计时长不满足追加条件".into()));
        }
        let mut result = Preview {
            package: p.clone(),
            paid_cents: p.price_cents * count as i64,
            status: "calculated".into(),
            reason: "提前结束".into(),
            ..Default::default()
        };
        let spec = self.spec_for(&p);
        let mut offer = {
            let rule = Rule {
                id: 1,
                station_id: 1,
                ..Default::default()
            };
            let offers = self.offers(&rule);
            offers[0].clone()
        };
        {
            let rule = Rule {
                id: 1,
                station_id: 1,
                ..Default::default()
            };
            for o in self.offers(&rule) {
                if o.package_id == id {
                    offer = o;
                }
            }
        }
        if count > 1 {
            offer.server_duration = true;
            offer.price_cents = result.paid_cents;
            offer.duration_minutes = (p.minutes as u16).saturating_mul(count);
            offer.purchase_count = count;
        }
        if p.mode == "amount"
            && let (Some(started), Some(ended)) = (meter.started_at, meter.ended_at)
            && ended - started >= chrono::Duration::minutes(n.policy.max_minutes as i64)
        {
            result.reason = "达到最长时长".into();
            let cutoff = started + chrono::Duration::minutes(n.policy.max_minutes as i64);
            match cutoff_meter(&spec, &meter, cutoff) {
                Ok(clipped) => meter = clipped,
                Err(_) => meter.review_required = true,
            }
        }
        // 仅在提供的实测读数处判断是否停机，不插补电量或预测读数间的变化。
        if p.mode == "amount" && !meter.review_required && !meter.segments.is_empty() {
            let rule = Rule {
                id: 1,
                version: 1,
                spec: spec.clone(),
                ..Default::default()
            };
            let seg_count = meter.segments.len();
            for i in 0..seg_count {
                let mut prefix = meter.clone();
                let at = meter.segments[i].ended_at;
                prefix.ended_at = Some(at);
                if let Some(started) = meter.started_at {
                    prefix.charged_seconds = (at - started).num_seconds().max(0) as u32;
                }
                prefix.segments = meter.segments[..=i].to_vec();
                prefix.charged_wh = prefix.segments.iter().map(|s| s.energy_wh).sum();
                match crate::charging_actual::price_actual(&rule, &prefix) {
                    Ok(fee) => {
                        if fee.total_cents >= p.price_cents {
                            meter = prefix;
                            result.cutoff_at = Some(at);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let offer_ref = offer.clone();
        let settled = match settle_session(
            &spec,
            &meter,
            Some(&offer_ref),
            Some(&actual_from_meter(&meter)),
        ) {
            Ok(s) => s,
            Err(BillingError::MeterReview) => {
                let mut r = result;
                r.status = "meter_review".into();
                r.reason = "无法可靠计算／待核对".into();
                return Ok(r);
            }
            Err(e) => return Err(SchemeError(e.to_string())),
        };
        result.settlement = settled.clone();
        if p.mode == "amount" {
            let mut raw_spec = spec.clone();
            raw_spec.free_minutes = 0;
            raw_spec.min_electric_cents = 0;
            let rule = Rule {
                id: 1,
                version: 1,
                spec: raw_spec.clone(),
                ..Default::default()
            };
            if let Ok(raw) = crate::charging_actual::price_actual(&rule, &meter) {
                result.raw_fee = Some(raw);
            }
            let rule2 = Rule {
                id: 1,
                version: 1,
                spec,
                ..Default::default()
            };
            if let Ok(adjusted) = crate::charging_actual::price_actual(&rule2, &meter) {
                result.policy_fee = Some(adjusted);
            }
        }
        result.refund_cents = result.paid_cents - settled.total_cents;
        if settled.total_cents >= result.paid_cents {
            result.reason = "套餐用尽／余额耗尽".into();
        }
        Ok(result)
    }

    /// 模拟启动失败、确认中或刷卡续购；wallet 为初始余额（分），仅计算预览（旧版 PreviewScenario）。
    pub fn preview_scenario(
        &self,
        id: u64,
        m: ActualMeter,
        scenario: &str,
        wallet: i64,
    ) -> Result<Preview, SchemeError> {
        self.validate()?;
        let p = self
            .package(id)
            .ok_or_else(|| SchemeError("请选择方案内套餐".into()))?;
        let n = self.clone().normalized();
        let mut r = Preview {
            package: p.clone(),
            paid_cents: p.price_cents,
            ..Default::default()
        };
        match scenario {
            "start_failed" => {
                r.status = "calculated".into();
                r.reason = "明确未启动，全额退款".into();
                r.refund_cents = p.price_cents;
                Ok(r)
            }
            "start_unknown" => {
                r.status = "confirming".into();
                r.reason = "启动确认中：查询／重放同一操作，保留端口占用".into();
                Ok(r)
            }
            "card_extend" | "card_failed" | "card_unknown" => {
                if p.mode != "duration" || n.card.package_id != id {
                    return Err(SchemeError("请选择明确指定的刷卡时长套餐".into()));
                }
                if wallet < p.price_cents {
                    r.status = "rejected".into();
                    r.reason = "首次刷卡余额不足，不扣费不启动".into();
                    return Ok(r);
                }
                let mut count: u16 = 2;
                let mut ops = vec!["首次扣款并确认启动，设备按累计上限执行".to_string()];
                if wallet < 2 * p.price_cents || (p.minutes as u32) * 2 > n.card.max_minutes as u32
                {
                    count = 1;
                    ops.push("余额或累计上限不足，整次拒绝加时".into());
                } else if scenario == "card_failed" {
                    count = 1;
                    ops.push("服务器拒绝本次加时，事务回滚，无扣款，原时长不变".into());
                } else if scenario == "card_unknown" {
                    r.paid_cents = 2 * p.price_cents;
                    r.status = "confirming".into();
                    r.reason = "模拟事务提交后应答丢失：按同一事件编号查询，不再次扣款".into();
                    r.operations
                        .push("查询同一事件的事务结果，重试不再次扣款".into());
                    r.wallet_after = Some(wallet - r.paid_cents);
                    return Ok(r);
                } else {
                    ops.push("服务器在同一事务中扣款并增加购买时长".into());
                }
                let mut result = self.preview_purchases(id, m, count)?;
                result.settlement.executor = "server".into();
                result.operations = ops;
                result.wallet_after = Some(wallet - result.settlement.total_cents);
                Ok(result)
            }
            "" => self.preview(id, m),
            _ => Err(SchemeError("预览状态无效".into())),
        }
    }
}

/// 协议支持的额度、上报数据、刷卡与停止功能（旧版 Capabilities）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Capabilities {
    pub reports_energy: bool,
    pub reports_segmented_power: bool,
    pub stop_when_full: bool,
    pub max_minutes: u16,
    pub duration: bool,
    pub energy: bool,
    pub online_card: bool,
}

impl Scheme {
    /// 校验方案所需功能与设备能力是否匹配（旧版 ValidateCapabilities）。
    pub fn validate_capabilities(&self, c: &Capabilities) -> Result<(), SchemeError> {
        self.validate()?;
        let n = self.clone().normalized();
        if n.stop.stop_when_full && !c.stop_when_full {
            return Err(SchemeError("设备协议不支持满充停止".into()));
        }
        let amount_is_energy_billing = self
            .amount
            .as_ref()
            .is_some_and(|a| a.algorithm == ChargeMode::new(ChargeMode::SERVER_ENERGY));
        if self.energy.is_some() || amount_is_energy_billing && !c.reports_energy {
            return Err(SchemeError("设备协议不支持电量上报".into()));
        }
        if let Some(amount) = &self.amount
            && amount.algorithm != ChargeMode::new(ChargeMode::SERVER_ENERGY)
            && !c.reports_segmented_power
        {
            return Err(SchemeError("设备协议不支持功率上报".into()));
        }
        for p in &n.packages {
            let mut minutes = p.minutes;
            if p.mode == "amount" {
                minutes = n.policy.max_minutes as u64;
            }
            if p.mode != "energy" && (!c.duration || minutes > c.max_minutes as u64) {
                return Err(SchemeError(format!(
                    "设备不支持套餐 {} 所需的{}分钟（设备上限{}分钟）",
                    p.name, minutes, c.max_minutes
                )));
            }
            if p.mode == "energy" && !c.energy {
                return Err(SchemeError("设备不支持电量执行".into()));
            }
        }
        if n.card.package_id != 0 && (!c.online_card || n.card.max_minutes > c.max_minutes) {
            return Err(SchemeError("设备协议不支持在线卡或所需累计时长".into()));
        }
        Ok(())
    }
}

// 兼容说明：计价纯函数 price_actual 位于 crate::charging_actual，由预览路径经 settle_session 间接使用。
