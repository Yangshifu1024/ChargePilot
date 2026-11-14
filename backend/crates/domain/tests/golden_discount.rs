#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 差分对照（辅缝 2）：以旧版 Go discountFor 批量产出的黄金样本逐条比对
//! Rust 实现。样本由旧仓库临时生成器一次性产出（生成期人工预期已核对，
//! 生成器即删），本测试只读快照。覆盖：三种优惠类型、阈值边界、
//! percentage 小数取整边界、减免封顶与参数缺失。

use chargepilot_domain::marketing::{DiscountError, DiscountType, GrantRule, discount_for};
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

/// 未知折扣类型（如黄金样本中的 cashback）在新范式下的等价行为：
/// 旧版 match 兜底返回 NotFound，新版在词表 parse 即拒绝——同一错误类别。
fn parse_of(input: &Value) -> Result<GrantRule, DiscountError> {
    let g: GrantIn = serde_json::from_value(input["grant"].clone()).expect("输入合法");
    let discount_type = DiscountType::parse(&g.kind).map_err(|_| DiscountError::NotFound)?;
    Ok(GrantRule {
        discount_type,
        min_charge_cents: g.min_charge,
        discount_value_cents: g.amount,
        discount_percent: g.percent,
        free_minutes: g.free_minutes,
    })
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
        let total = input["total"].as_i64().unwrap();
        let grant = match parse_of(input) {
            Ok(g) => g,
            Err(e) => {
                // 未知类型：新旧行为等价为 NotFound，按样本预期断言错误类别。
                assert_eq!(
                    e,
                    want_err_of(case["want_err"].as_str().unwrap_or("未知类型")),
                    "{id}: 未知类型错误类别不一致"
                );
                checked += 1;
                continue;
            }
        };
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
