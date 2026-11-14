#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 差分对照（辅缝 2）：以旧版 Go discountFor 批量产出的黄金样本逐条比对
//! Rust 实现。样本由旧仓库临时生成器一次性产出（生成期人工预期已核对，
//! 生成器即删），本测试只读快照。覆盖：三种优惠类型、阈值边界、
//! percentage 小数取整边界、减免封顶与参数缺失。

use chargepilot_domain::marketing::{DiscountError, GrantRule, discount_for};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct GrantIn {
    #[serde(rename = "type")]
    kind: String,
    min_charge: i64,
    #[serde(default)]
    amount: Option<i64>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    free_minutes: u32,
}

fn grant_of(input: &Value) -> GrantRule {
    let g: GrantIn = serde_json::from_value(input["grant"].clone()).expect("输入合法");
    GrantRule {
        discount_type: g.kind,
        min_charge_cents: g.min_charge,
        discount_value_cents: g.amount,
        discount_percent: g.percent,
        free_minutes: g.free_minutes,
    }
}

fn want_err_of(want: &str) -> DiscountError {
    if want.contains("门槛") {
        DiscountError::Threshold
    } else {
        DiscountError::NotFound
    }
}

#[test]
fn discount_golden_matches_legacy() {
    let raw = include_str!("golden/discount.json");
    let file: serde_json::Value = serde_json::from_str(raw).expect("黄金样本 JSON 合法");
    let cases = file["cases"].as_array().expect("cases 数组");
    assert!(
        cases.len() >= 30,
        "样本应覆盖足够分支：实际 {}",
        cases.len()
    );
    let mut checked = 0;
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let input = &case["input"];
        let grant = grant_of(input);
        let total = input["total"].as_i64().unwrap();
        match discount_for(&grant, total) {
            Ok(got) => {
                let want = case["want"].as_i64().unwrap_or_else(|| {
                    panic!("{id}: 期望成功但样本无 want 值");
                });
                assert_eq!(got, want, "{id}: 优惠金额与旧版不一致");
            }
            Err(e) => {
                assert!(
                    !case["want_err"].as_str().unwrap_or("").is_empty(),
                    "{id}: 意外成功"
                );
                assert_eq!(
                    e,
                    want_err_of(case["want_err"].as_str().unwrap()),
                    "{id}: 错误类别不一致"
                );
            }
        }
        checked += 1;
    }
    assert_eq!(checked, cases.len());
}
