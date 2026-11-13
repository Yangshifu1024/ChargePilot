//! 差分对照（辅缝 2）：以旧版 Go 计费纯函数批量产出的黄金样本逐条比对 Rust 实现。
//! 样本由旧仓库 cmd/goldengen 一次性生成（生成期人工预期已核对），本测试只读快照。
//! 覆盖：Cost / PriceActual / SettleSession / MeasuredSegments / Estimate /
//! BudgetStop / CutoffMeter / DecideStop / ValidateSpec / Scheme 校验。

use chargepilot_domain::charging_actual::{price_actual, ActualMeter, MeterSegment, Rule};
use chargepilot_domain::charging_cost::Fee;
use chargepilot_domain::charging_measured::{measured_segments, Reading, ReadingSample, SessionSpan};
use chargepilot_domain::charging_scheme::Scheme;
use chargepilot_domain::charging_settle::{
    budget_stop_at_meter, decide_stop, estimate_charge, settle_session, ControlInstruction, Offer,
    SessionActual, Settlement,
};
use chargepilot_domain::charging_spec::{validate_spec, BillingError, Spec, Usage};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct GoldenCase {
    id: String,
    kind: String,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    want: Value,
    #[serde(default)]
    want_err: String,
}

#[derive(Deserialize)]
struct GoldenFile {
    cases: Vec<GoldenCase>,
}

/// Go 空切片 marshal 为 null：反序列化时归一为空 Vec。
fn null_to_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    let v: Option<Vec<T>> = Option::deserialize(deserializer)?;
    Ok(v.unwrap_or_default())
}

fn golden() -> &'static Vec<GoldenCase> {
    use std::sync::OnceLock;
    static GOLDEN: OnceLock<Vec<GoldenCase>> = OnceLock::new();
    GOLDEN.get_or_init(|| {
        let raw = include_str!("golden/billing.json");
        serde_json::from_str::<GoldenFile>(raw)
            .expect("黄金样本 JSON 合法")
            .cases
    })
}

fn want_err_of(want: &str) -> Option<BillingError> {
    match want {
        "" => None,
        "invalid_pricing" => Some(BillingError::InvalidPricing),
        "meter_review" => Some(BillingError::MeterReview),
        "not_server_billed" => Some(BillingError::NotServerBilled),
        "no_paid_amount" => Some(BillingError::NoPaidAmount),
        other => panic!("未知错误名: {other}"),
    }
}

/// 与旧版哨兵错误等价比较（只比较变体，不比较 Display 文案）。
fn assert_err(case_id: &str, got: Result<impl Sized, BillingError>, want: &str) {
    match (got, want_err_of(want)) {
        (Ok(_), None) => {}
        (Ok(_), Some(e)) => panic!("[{case_id}] 期待错误 {e:?} 却成功"),
        (Err(got), None) => panic!("[{case_id}] 期待成功却得到错误 {got:?}"),
        (Err(got), Some(want)) => assert_eq!(
            got, want,
            "[{case_id}] 错误变体与旧版不一致"
        ),
    }
}

// ---------- 输入侧反序列化桥 ----------

#[derive(Deserialize)]
struct WireUsage {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    energy_wh: u64,
    #[serde(default, deserialize_with = "null_to_empty")]
    samples: Vec<WireSample>,
}

#[derive(Deserialize)]
struct WireSample {
    #[serde(default)]
    power_known: bool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[serde(default)]
    energy_wh: u64,
    #[serde(default)]
    power_w: u32,
}

impl WireUsage {
    fn into_usage(self) -> Usage {
        Usage {
            start: self.start,
            end: self.end,
            energy_wh: self.energy_wh,
            samples: self
                .samples
                .into_iter()
                .map(|s| chargepilot_domain::charging_spec::Sample {
                    power_known: s.power_known,
                    start: s.start,
                    end: s.end,
                    energy_wh: s.energy_wh,
                    power_w: s.power_w,
                })
                .collect(),
            channel: None,
        }
    }
}

#[derive(Deserialize)]
struct WireMeter {
    #[serde(default)]
    review_required: bool,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    charged_wh: u32,
    #[serde(default)]
    charged_seconds: u32,
    #[serde(default, deserialize_with = "null_to_empty")]
    segments: Vec<WireSegment>,
}

impl WireMeter {
    fn into_meter(self) -> ActualMeter {
        ActualMeter {
            review_required: self.review_required,
            started_at: self.started_at,
            ended_at: self.ended_at,
            charged_wh: self.charged_wh,
            charged_seconds: self.charged_seconds,
            segments: self.segments.into_iter().map(|s| s.into_segment()).collect(),
        }
    }
}

#[derive(Deserialize, Debug)]
struct WireSegment {
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    #[serde(default)]
    energy_wh: u32,
    #[serde(default)]
    peak_w: u32,
    #[serde(default)]
    power_w: Option<u32>,
}

impl WireSegment {
    fn into_segment(self) -> MeterSegment {
        MeterSegment {
            started_at: self.started_at,
            ended_at: self.ended_at,
            energy_wh: self.energy_wh,
            peak_w: self.peak_w,
            power_w: self.power_w,
        }
    }
}

/// Go 侧 JSON 对象（spec/rule/offer 等带 json tag 的结构直接透传）。
fn parse_spec(v: &Value) -> Spec {
    serde_json::from_value(v.clone()).expect("spec 反序列化")
}

fn parse_rule(v: &Value) -> Rule {
    serde_json::from_value(v.clone()).expect("rule 反序列化")
}

/// Fee 比对：fragments 的时间按 DateTime 语义比较（字符串格式两端可能不同），其余逐字段。
fn assert_fee_eq(case_id: &str, got: &Fee, want: &Value) {
    let want_fee: Fee = serde_json::from_value(want.clone()).expect("want Fee 合法");
    assert_eq!(got.basis, want_fee.basis, "[{case_id}] basis");
    assert_eq!(got.electric_cents, want_fee.electric_cents, "[{case_id}] electric_cents");
    assert_eq!(got.service_cents, want_fee.service_cents, "[{case_id}] service_cents");
    assert_eq!(got.total_cents, want_fee.total_cents, "[{case_id}] total_cents");
    assert_eq!(got.billable_wh, want_fee.billable_wh, "[{case_id}] billable_wh");
    assert_eq!(
        got.fragments.len(),
        want_fee.fragments.len(),
        "[{case_id}] fragments 数量: got={:?} want={:?}",
        got.fragments,
        want_fee.fragments
    );
    for (g, w) in got.fragments.iter().zip(want_fee.fragments.iter()) {
        let parse = |s: &str| -> DateTime<Utc> {
            DateTime::parse_from_rfc3339(s).expect("时间可解析").with_timezone(&Utc)
        };
        assert_eq!(parse(&g.started_at), parse(&w.started_at), "[{case_id}] fragment started_at");
        assert_eq!(parse(&g.ended_at), parse(&w.ended_at), "[{case_id}] fragment ended_at");
        assert_eq!(g.power_w, w.power_w, "[{case_id}] fragment power_w");
        assert_eq!(g.quantity, w.quantity, "[{case_id}] fragment quantity");
        assert_eq!(g.unit, w.unit, "[{case_id}] fragment unit");
        assert_eq!(g.electric_rate, w.electric_rate, "[{case_id}] fragment electric_rate");
        assert_eq!(g.service_rate, w.service_rate, "[{case_id}] fragment service_rate");
        assert_eq!(g.electric_cents, w.electric_cents, "[{case_id}] fragment electric_cents");
        assert_eq!(g.service_cents, w.service_cents, "[{case_id}] fragment service_cents");
    }
}

// ---------- 各 kind 执行器 ----------

fn run_case(c: &GoldenCase) {
    match c.kind.as_str() {
        "cost" => {
            let spec = parse_spec(&c.input["spec"]);
            let usage: WireUsage =
                serde_json::from_value(c.input["usage"].clone()).expect("usage");
            let got = chargepilot_domain::charging_cost::cost(&spec, &usage.into_usage());
            match (&got, want_err_of(&c.want_err)) {
                (Ok(fee), None) => assert_fee_eq(&c.id, fee, &c.want),
                (got, _want) => assert_err(&c.id, got.clone().map(|_| ()), &c.want_err),
            }
        }
        "price_actual" => {
            let rule = parse_rule(&c.input["rule"]);
            let meter: WireMeter =
                serde_json::from_value(c.input["meter"].clone()).expect("meter");
            let got = price_actual(&rule, &meter.into_meter());
            match (&got, want_err_of(&c.want_err)) {
                (Ok(fee), None) => assert_fee_eq(&c.id, fee, &c.want),
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "settle" => {
            let spec = parse_spec(&c.input["spec"]);
            let meter: WireMeter =
                serde_json::from_value(c.input["meter"].clone()).expect("meter");
            let offer: Option<Offer> = match &c.input["offer"] {
                Value::Null => None,
                v => Some(serde_json::from_value(v.clone()).expect("offer")),
            };
            let actual: Option<SessionActual> = match &c.input["actual"] {
                Value::Null => None,
                v => Some(serde_json::from_value(v.clone()).expect("actual")),
            };
            let got = settle_session(&spec, &meter.into_meter(), offer.as_ref(), actual.as_ref());
            match (&got, want_err_of(&c.want_err)) {
                (Ok(s), None) => {
                    let want: Settlement =
                        serde_json::from_value(c.want.clone()).expect("want Settlement");
                    assert_eq!(s, &want, "[{}] Settlement 不一致", c.id);
                }
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "measured" => {
            let start: DateTime<Utc> =
                serde_json::from_value(c.input["start"].clone()).expect("start");
            let span: SessionSpanWire =
                serde_json::from_value(c.input["span"].clone()).expect("span");
            let samples: Vec<WireReadingSample> =
                serde_json::from_value(c.input["samples"].clone()).expect("samples");
            let got = measured_segments(start, &span.into_span(), &samples.into_iter().map(|s| s.into_sample()).collect::<Vec<_>>());
            let want: Option<Vec<WireSegment>> = serde_json::from_value(c.want.clone()).expect("want segments");
            match (&got, want) {
                (Some(gs), Some(ws)) => {
                    assert_eq!(gs.len(), ws.len(), "[{}] 分段数量", c.id);
                    for (g, w) in gs.iter().zip(ws.iter()) {
                        assert_eq!(g.started_at, w.started_at, "[{}] started_at", c.id);
                        assert_eq!(g.ended_at, w.ended_at, "[{}] ended_at", c.id);
                        assert_eq!(g.energy_wh, w.energy_wh, "[{}] energy_wh", c.id);
                        assert_eq!(g.peak_w, w.peak_w, "[{}] peak_w", c.id);
                        assert_eq!(g.power_w, w.power_w, "[{}] power_w", c.id);
                    }
                }
                (None, None) => {}
                (got, want) => panic!(
                    "[{}] measured 差分失败: got={got:?} want={want:?}",
                    c.id
                ),
            }
        }
        "estimate" => {
            let rule = parse_rule(&c.input["rule"]);
            let energy = c.input["energy"].as_str().unwrap();
            let minutes = c.input["minutes"].as_u64().unwrap() as u16;
            let start: DateTime<Utc> =
                serde_json::from_value(c.input["start"].clone()).expect("start");
            let prepaid = c.input["prepaid_cents"].as_i64().unwrap_or(0);
            let got = estimate_charge(&rule, energy, minutes, start, prepaid);
            match (&got, want_err_of(&c.want_err)) {
                (Ok(e), None) => {
                    let want: chargepilot_domain::charging_settle::Estimate =
                        serde_json::from_value(c.want.clone()).expect("want Estimate");
                    assert_eq!(e, &want, "[{}] Estimate 不一致", c.id);
                }
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "budget" => {
            let rule = parse_rule(&c.input["rule"]);
            let offer: Offer =
                serde_json::from_value(c.input["offer"].clone()).expect("offer");
            let meter: WireMeter =
                serde_json::from_value(c.input["meter"].clone()).expect("meter");
            let got = budget_stop_at_meter(&rule, Some(&offer), &meter.into_meter());
            match (&got, want_err_of(&c.want_err)) {
                (Ok((plan, fee)), None) => {
                    let want_plan: WirePlan =
                        serde_json::from_value(c.want["plan"].clone()).expect("want plan");
                    assert_eq!(plan.should_stop, want_plan.should_stop, "[{}] should_stop", c.id);
                    assert_eq!(plan.reason, want_plan.reason, "[{}] reason", c.id);
                    assert_eq!(plan.accrued_cents, want_plan.accrued_cents, "[{}] accrued_cents", c.id);
                    assert_eq!(plan.grace_seconds, want_plan.grace_seconds, "[{}] grace_seconds", c.id);
                    match (&c.want["fee"], fee) {
                        (Value::Null, None) => {}
                        (v, Some(f)) => {
                            let want_fee: Fee =
                                serde_json::from_value(v.clone()).expect("want fee");
                            assert_eq!(f.electric_cents, want_fee.electric_cents, "[{}] fee.electric", c.id);
                            assert_eq!(f.service_cents, want_fee.service_cents, "[{}] fee.service", c.id);
                            assert_eq!(f.total_cents, want_fee.total_cents, "[{}] fee.total", c.id);
                        }
                        _ => panic!("[{}] fee 存在性不一致", c.id),
                    }
                }
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "cutoff" => {
            let spec = parse_spec(&c.input["spec"]);
            let meter: WireMeter =
                serde_json::from_value(c.input["meter"].clone()).expect("meter");
            let cutoff: DateTime<Utc> =
                serde_json::from_value(c.input["cutoff"].clone()).expect("cutoff");
            let got = chargepilot_domain::charging_cost::cutoff_meter(&spec, &meter.into_meter(), cutoff);
            match (&got, want_err_of(&c.want_err)) {
                (Ok(m), None) => {
                    let want: WireMeter =
                        serde_json::from_value(c.want.clone()).expect("want meter");
                    assert_eq!(m.review_required, want.review_required, "[{}] review_required", c.id);
                    assert_eq!(m.started_at, want.started_at, "[{}] started_at", c.id);
                    assert_eq!(m.ended_at, want.ended_at, "[{}] ended_at", c.id);
                    assert_eq!(m.charged_wh, want.charged_wh, "[{}] charged_wh", c.id);
                    assert_eq!(m.charged_seconds, want.charged_seconds, "[{}] charged_seconds", c.id);
                    assert_eq!(m.segments.len(), want.segments.len(), "[{}] segments 数量", c.id);
                    for (g, w) in m.segments.iter().zip(want.segments.iter()) {
                        assert_eq!(g.started_at, w.started_at, "[{}] seg started_at", c.id);
                        assert_eq!(g.ended_at, w.ended_at, "[{}] seg ended_at", c.id);
                        assert_eq!(g.energy_wh, w.energy_wh, "[{}] seg energy_wh", c.id);
                        assert_eq!(g.peak_w, w.peak_w, "[{}] seg peak_w", c.id);
                        assert_eq!(g.power_w, w.power_w, "[{}] seg power_w", c.id);
                    }
                }
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "decide_stop" => {
            let spec = parse_spec(&c.input["spec"]);
            let usage: WireUsage =
                serde_json::from_value(c.input["usage"].clone()).expect("usage");
            let elapsed = chrono::Duration::seconds(c.input["elapsed_seconds"].as_i64().unwrap());
            let got = decide_stop(&spec, &usage.into_usage(), elapsed);
            match (&got, want_err_of(&c.want_err)) {
                (Ok(plan), None) => {
                    // Go 侧 StopPlan 无 json tag（PascalCase 字段名），用桥接结构比较
                    let want: WirePlan =
                        serde_json::from_value(c.want.clone()).expect("want plan");
                    assert_eq!(plan.should_stop, want.should_stop, "[{}] should_stop", c.id);
                    assert_eq!(plan.reason, want.reason, "[{}] reason", c.id);
                    assert_eq!(plan.accrued_cents, want.accrued_cents, "[{}] accrued_cents", c.id);
                    assert_eq!(plan.grace_seconds, want.grace_seconds, "[{}] grace_seconds", c.id);
                }
                _ => assert_err(&c.id, got.map(|_| ()), &c.want_err),
            }
        }
        "validate_spec" => {
            let spec = parse_spec(&c.input["spec"]);
            let got = validate_spec(&spec);
            assert_err(&c.id, got.map(|_| ()), &c.want_err);
        }
        "validate_scheme" => {
            let scheme: Scheme =
                serde_json::from_value(c.input["scheme"].clone()).expect("scheme");
            let got = scheme.validate();
            match (got, c.want_err.as_str()) {
                (Ok(()), "") => {}
                (Ok(()), _) => panic!("[{}] 期待错误 {} 却成功", c.id, c.want),
                (Err(e), "") => panic!("[{}] 期待成功却得到错误 {e}", c.id),
                (Err(e), "scheme_err") => {
                    assert_eq!(e.to_string(), c.want.as_str().unwrap(), "[{}] 校验文案不一致", c.id);
                }
                (Err(e), other) => panic!("[{}] 未知 want_err={other}（err={e}）", c.id),
            }
        }
        other => panic!("未知 kind: {other}"),
    }
}

#[derive(Deserialize)]
struct SessionSpanWire {
    device_id: String,
    port: u8,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    charged_seconds: u32,
    energy_wh: u32,
}

impl SessionSpanWire {
    fn into_span(self) -> SessionSpan {
        SessionSpan {
            device_id: self.device_id,
            port: self.port,
            started_at: self.started_at,
            ended_at: self.ended_at,
            charged_seconds: self.charged_seconds,
            energy_wh: self.energy_wh,
        }
    }
}

#[derive(Deserialize)]
struct WireReadingSample {
    device_id: String,
    received_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "null_to_empty")]
    readings: Vec<WireReading>,
}

impl WireReadingSample {
    fn into_sample(self) -> ReadingSample {
        ReadingSample {
            device_id: self.device_id,
            received_at: self.received_at,
            readings: self
                .readings
                .into_iter()
                .map(|r| Reading {
                    port: r.port,
                    charged_seconds: r.charged_seconds,
                    charged_mwh: r.charged_mwh,
                    power_w: r.power_w,
                })
                .collect(),
        }
    }
}

/// Go 侧 StopPlan 无 json tag：字段按导出名（PascalCase）反序列化的桥接结构。
#[derive(Deserialize)]
struct WirePlan {
    #[serde(rename = "ShouldStop", default)]
    should_stop: bool,
    #[serde(rename = "Reason", default)]
    reason: String,
    #[serde(rename = "AccruedCents", default)]
    accrued_cents: i64,
    #[serde(rename = "GraceSeconds", default)]
    grace_seconds: i32,
}

#[derive(Deserialize)]
struct WireReading {
    port: u8,
    charged_seconds: u32,
    charged_mwh: u32,
    power_w: u32,
}

/// StopPlan 需要 PartialEq（差分比较）；此处不新增方法，直接引用派生。
#[allow(unused_imports)]
use chargepilot_domain::charging_settle as _settle_reexports;

/// 每条黄金样本一个独立测试（失败时精确定位）。
#[test]
fn golden_differential_all_cases() {
    let cases = golden();
    assert!(cases.len() >= 80, "黄金样本数量异常: {}", cases.len());
    let mut failed = Vec::new();
    for c in cases {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_case(c);
        }));
        if let Err(e) = result {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "未知 panic".to_string());
            failed.push(format!("{}: {msg}", c.id));
        }
    }
    if !failed.is_empty() {
        panic!(
            "差分对照 {} 条失败:\n{}",
            failed.len(),
            failed.join("\n")
        );
    }
}

/// ControlInstruction::valid 的单测（无 Go 差分——旧版 Valid 纯结构校验，语义简单）。
#[test]
fn control_instruction_validity() {
    let mut c = ControlInstruction {
        mode: chargepilot_domain::charging_spec::ChargeMode::new(
            chargepilot_domain::charging_spec::ChargeMode::DEVICE_DURATION,
        ),
        minutes: 60,
        ..Default::default()
    };
    assert!(c.valid());
    c.minutes = 4321;
    assert!(!c.valid());
    c.minutes = 0;
    assert!(!c.valid());
    c.mode = chargepilot_domain::charging_spec::ChargeMode::new(
        chargepilot_domain::charging_spec::ChargeMode::DEVICE_POWER,
    );
    c.balance_cents = 500;
    c.tier_cents_per_hour = vec![30, 50];
    assert!(c.valid());
    c.tier_cents_per_hour.clear();
    assert!(!c.valid());
}
