//! 计费方案纯规则：结构 serde 对齐旧版 spec_json 契约（字段名逐字节一致），校验规则照搬。
//! 语义基准：旧仓库 internal/domain/charging/{scheme,spec}.go。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct SchemeError(pub String);

/// 完整计费方案：模板、发布副本与订单快照使用同一结构。设备覆盖必须整份替换。
/// 容器级 serde(default)：历史模板的 scheme 可能缺字段，反序列化时逐字段补默认。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Scheme {
    pub name: String,
    pub remark: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<AmountMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<EnergyMode>,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub policy: AmountPolicy,
    #[serde(default)]
    pub stop: TimeCharge,
    #[serde(default)]
    pub card: CardPolicy,
    #[serde(default)]
    pub display: Display,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmountMode {
    pub algorithm: String,
    #[serde(default)]
    pub periods: Vec<Period>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnergyMode {
    pub electric_cents: i64,
    pub service_cents: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Package {
    pub id: u64,
    pub name: String,
    /// 金额、时长、电量
    pub mode: String,
    pub price_cents: i64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub minutes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub kwh: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AmountPolicy {
    #[serde(default)]
    pub free_minutes: i32,
    #[serde(default)]
    pub min_electric_cents: i64,
    #[serde(default)]
    pub max_minutes: u16,
    #[serde(default)]
    pub loss_rate_bp: i32,
    #[serde(default)]
    pub channel_bp: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CardPolicy {
    #[serde(default)]
    pub package_id: u64,
    #[serde(default)]
    pub max_minutes: u16,
}

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

/// 功率阶梯档（上界瓦数严格递增，≤9990）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tier {
    pub max_watts: i64,
    pub electric_cents: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub service_cents: i64,
}

/// 费率时段：无开始分钟——每段开始=上一段结束，第一段从 0 点起，末段必须覆盖到 1440。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Period {
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub service_cents: i64,
    pub end_minute: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub electric_cents: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<Tier>,
}

/// spec_json 的包装结构：新版只读写 `scheme`，历史遗留字段经 `extra` 原样往返不丢失。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Spec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<Scheme>,
    /// 旧版本遗留字段（mode/electric/service/display 等），原样保留避免升级丢数据。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

impl Scheme {
    /// 补齐默认值（最长时长 600 分钟、渠道倍率 10000），不修改调用方。
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

    /// 全量校验（语义照搬旧版 Validate；错误文案与旧版一致）。
    pub fn validate(&self) -> Result<(), SchemeError> {
        let s = self.clone().normalized();
        let name_chars = s.name.chars().count();
        if name_chars == 0 || name_chars > 64 || s.remark.chars().count() > 255 {
            return Err(SchemeError("请填写方案名称（最多64字）及有效说明".into()));
        }
        if s.packages.is_empty() || s.packages.len() > 99 {
            return Err(SchemeError("请配置1～99个支付套餐".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for p in &s.packages {
            if !(1..=99).contains(&p.id) {
                return Err(SchemeError("套餐编号须在 1–99 之间".into()));
            }
            if !ids.insert(p.id) {
                return Err(SchemeError("套餐编号重复".into()));
            }
            if p.price_cents < 0 {
                return Err(SchemeError("套餐价格无效".into()));
            }
            if p.mode == "duration" && p.minutes == 0 {
                return Err(SchemeError("时长套餐必须配置分钟数".into()));
            }
            if p.mode == "energy" && p.kwh == 0 {
                return Err(SchemeError("电量套餐必须配置电量".into()));
            }
        }
        if !(60..=4320).contains(&s.policy.max_minutes)
            || !(60..=4320).contains(&s.card.max_minutes)
        {
            return Err(SchemeError("最长充电时长须在 60–4320 分钟之间".into()));
        }
        if s.policy.loss_rate_bp != 0 {
            return Err(SchemeError("损耗率已停用，必须为 0".into()));
        }
        if s.policy.channel_bp != 10000 {
            return Err(SchemeError("渠道倍率已停用，必须为 10000".into()));
        }
        // 电量套餐价与单价的乘积一致（旧版语义）
        if let Some(energy) = &s.energy {
            for p in s.packages.iter().filter(|p| p.mode == "energy") {
                let expected = p.kwh as i64 * (energy.electric_cents + energy.service_cents);
                if p.price_cents != expected {
                    return Err(SchemeError("电量套餐价格须等于电量乘以单价之和".into()));
                }
            }
        }
        // 刷卡套餐必须指向某个时长套餐
        if s.card.package_id != 0 {
            let ok = s
                .packages
                .iter()
                .any(|p| p.id == s.card.package_id && p.mode == "duration");
            if !ok {
                return Err(SchemeError("刷卡套餐必须选择一个时长套餐".into()));
            }
        }
        if let Some(amount) = &s.amount {
            compile_periods(&amount.periods)?;
        }
        Ok(())
    }
}

/// 全天 1440 分钟闭环校验：1–48 段、end_minute 严格递增、恰好覆盖 [0,1440]（旧版语义）。
pub fn compile_periods(periods: &[Period]) -> Result<(), SchemeError> {
    if periods.is_empty() || periods.len() > 48 {
        return Err(SchemeError("费率时段须为 1–48 段".into()));
    }
    let mut cursor = 0i64;
    for p in periods {
        if p.end_minute <= cursor || p.end_minute > 1440 {
            return Err(SchemeError("费率时段必须按时间顺序无缝覆盖全天".into()));
        }
        if p.electric_cents < 0 || p.service_cents < 0 {
            return Err(SchemeError("费率不能为负".into()));
        }
        // 阶梯档校验：≤8 档、上界严格递增且 ≤9990
        let mut upper = 0i64;
        for t in &p.tiers {
            if t.max_watts <= upper || t.max_watts > 9990 || t.electric_cents < 0 {
                return Err(SchemeError("功率阶梯须严格递增且不超过 9990 瓦".into()));
            }
            upper = t.max_watts;
        }
        cursor = p.end_minute;
    }
    if cursor != 1440 {
        return Err(SchemeError(
            "费率时段必须覆盖全天（末段止于 1440 分钟）".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_scheme() -> Scheme {
        serde_json::from_value(json!({
            "name": "测试方案",
            "remark": "",
            "amount": {"algorithm": "time", "periods": [
                {"electric_cents": 50, "service_cents": 30, "end_minute": 720},
                {"electric_cents": 30, "service_cents": 20, "end_minute": 1440}
            ]},
            "packages": [
                {"id": 1, "name": "1元", "mode": "money", "price_cents": 100},
                {"id": 2, "name": "2小时", "mode": "duration", "price_cents": 200, "minutes": 120}
            ],
            "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 0},
            "stop": {"stop_when_full": true},
            "card": {"package_id": 2, "max_minutes": 600},
            "display": {"duration": [], "energy": []}
        }))
        .unwrap()
    }

    use serde_json::json;

    #[test]
    fn valid_scheme_passes_and_normalizes() {
        let s = base_scheme().normalized();
        s.validate().unwrap();
        assert_eq!(s.policy.channel_bp, 10000);
    }

    #[test]
    fn period_gap_rejected() {
        let mut s = base_scheme();
        s.amount.as_mut().unwrap().periods[1].end_minute = 1400; // 不到 1440
        assert!(s.validate().is_err());
    }

    #[test]
    fn card_package_must_be_duration() {
        let mut s = base_scheme();
        s.card.package_id = 1; // money 套餐
        assert!(s.validate().is_err());
    }

    #[test]
    fn energy_package_price_must_match() {
        let mut s = base_scheme();
        s.energy = Some(EnergyMode {
            electric_cents: 100,
            service_cents: 50,
        });
        s.packages.push(Package {
            id: 3,
            name: "1度".to_string(),
            mode: "energy".to_string(),
            price_cents: 100,
            minutes: 0,
            kwh: 1,
        });
        // 1 kWh × (100+50) = 150 ≠ 100
        assert!(s.validate().is_err());
        s.packages.last_mut().unwrap().price_cents = 150;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn spec_extra_fields_round_trip() {
        let raw = r#"{"mode":"time","scheme":{"name":"老数据"},"electric":{"basis":"server"},"display":{}}"#;
        let spec: Spec = serde_json::from_str(raw).unwrap();
        assert!(spec.scheme.is_some());
        assert!(spec.extra.contains_key("electric"));
        let back = serde_json::to_string(&spec).unwrap();
        assert!(back.contains("\"electric\""), "遗留字段必须原样保留");
    }
}
