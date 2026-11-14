//! 支付意图与订单编号的纯业务规则（零 IO，旧版 payment 域规则对齐）。
//! 意图状态词表、支付前预估量计算、迟到判定与两套业务单号格式都在这里；
//! 事务编排与表访问在 modules/payment 与 modules/order。

use crate::charging_scheme::{Package, Scheme};
use crate::charging_settle::Estimate;
use crate::charging_spec::beijing;
use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// 支付意图状态（charge_payment_intent.status 列，五值）。
    /// 与 [`crate::money::PaymentOrderStatus`] 分立：共享 initiated/paid 字面量
    /// 但含义不同，不得互相赋值（票 09 对照表裁决的唯一属主落位本域）。
    /// 生成列 `active_port_code` 不建 Rust 类型，由数据库派生。
    // 取值语义：initiated 已创建待支付（占用端口）；paid 已支付并关联充电订单；
    // expired 到期未支付（预占释放）；closed 手工关闭；refund_required 支付迟到需原路退款。
    PaymentIntentStatus: {
        Initiated = "initiated",
        Paid = "paid",
        Expired = "expired",
        Closed = "closed",
        RefundRequired = "refund_required",
    }
);

/// 支付意图业务冲突（旧版 ErrPaymentIntentConflict 的语义族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    /// 输入不合法或与已存在意图不一致。
    Conflict,
    /// 套餐不可售或与当前规则不匹配。
    OfferUnavailable,
    /// 计费方案快照无效。
    InvalidPricing,
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            IntentError::Conflict => "支付申请冲突",
            IntentError::OfferUnavailable => "套餐不可售",
            IntentError::InvalidPricing => "计费方案无效",
        };
        f.write_str(text)
    }
}

impl std::error::Error for IntentError {}

/// 支付前冻结的预估量与设备执行量（旧版 Reserve 的 estimate 分支）。
/// 时长套餐下发购买分钟（charge_mode=0），电量套餐下发 Wh（charge_mode=1），
/// 金额套餐下发最长时长（charge_mode=4）并由平台检查费用上限。
pub fn intent_estimate(
    scheme: &Scheme,
    package: &Package,
    offer_price_cents: i64,
) -> Result<Estimate, IntentError> {
    if offer_price_cents != package.price_cents || package.price_cents <= 0 {
        return Err(IntentError::InvalidPricing);
    }
    let normalized = scheme.clone().normalized();
    let max_minutes = normalized.policy.max_minutes;
    let (minutes, charge_mode, quantity) = match package.mode.as_str() {
        "duration" => {
            if package.minutes == 0 {
                return Err(IntentError::InvalidPricing);
            }
            (package.minutes as u16, 0u8, package.minutes as u16)
        }
        "energy" => {
            if package.kwh == 0 {
                return Err(IntentError::InvalidPricing);
            }
            (0u16, 1u8, package.kwh as u16)
        }
        "amount" => (max_minutes, 4u8, max_minutes),
        _ => return Err(IntentError::InvalidPricing),
    };
    Ok(Estimate {
        estimated_kwh: "0.000".to_string(),
        estimated_minutes: minutes,
        mode: crate::charging_spec::ChargeMode::default(),
        basis: String::new(),
        electric_cents: 0,
        service_cents: 0,
        total_cents: package.price_cents,
        prepaid_cents: offer_price_cents,
        charge_mode,
        charge_quantity: quantity,
    })
}

/// 迟到支付判定：当前时刻或到账时刻越过支付期限即视为迟到（旧版 Apply）。
pub fn is_late_payment(
    now_unix_micros: i64,
    paid_at_unix_micros: i64,
    expires_at_unix_micros: i64,
) -> bool {
    now_unix_micros > expires_at_unix_micros || paid_at_unix_micros > expires_at_unix_micros
}

/// 充电业务单号：C + 北京时间 14 位时间戳 + 设备编号 + 两位端口号（旧版 ChargeOrderNumber）。
/// 设备编号仅允许字母、数字、下划线与连字符；总长不超过 64 字节。
pub fn charge_order_no(
    at_unix_micros: i64,
    device: &str,
    port_no: u8,
) -> Result<String, IntentError> {
    if device.is_empty() || port_no == 0 || port_no > 99 {
        return Err(IntentError::Conflict);
    }
    if !device
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(IntentError::Conflict);
    }
    let stamp = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(at_unix_micros)
        .ok_or(IntentError::Conflict)?
        .with_timezone(&beijing())
        .format("%Y%m%d%H%M%S")
        .to_string();
    let no = format!("C{stamp}{device}{port_no:02}");
    if no.len() > 64 {
        return Err(IntentError::Conflict);
    }
    Ok(no)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-14 12:30:45 北京时间（= 04:30:45 UTC）= 1794630645 秒。
    const AT_MICROS: i64 = 1_794_630_645 * 1_000_000;

    #[test]
    fn charge_no_format() {
        let no = charge_order_no(AT_MICROS, "DEV-001", 3).unwrap();
        assert_eq!(no, "C20261114123045DEV-00103");
    }

    #[test]
    fn charge_no_rejects_bad_device() {
        assert_eq!(
            charge_order_no(AT_MICROS, "", 1),
            Err(IntentError::Conflict)
        );
        assert_eq!(
            charge_order_no(AT_MICROS, "D/E", 1),
            Err(IntentError::Conflict)
        );
        assert_eq!(
            charge_order_no(AT_MICROS, "D", 0),
            Err(IntentError::Conflict)
        );
        assert_eq!(
            charge_order_no(AT_MICROS, "D", 100),
            Err(IntentError::Conflict)
        );
    }

    #[test]
    fn late_payment_boundaries() {
        let expires = 1_000_000;
        assert!(!is_late_payment(expires, expires, expires));
        assert!(is_late_payment(expires + 1, expires, expires));
        assert!(is_late_payment(expires, expires + 1, expires));
    }
}

#[cfg(test)]
mod status_tests {
    use super::PaymentIntentStatus;

    /// 字面量对照（票 09 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let intents = [
            (PaymentIntentStatus::Initiated, "initiated"),
            (PaymentIntentStatus::Paid, "paid"),
            (PaymentIntentStatus::Expired, "expired"),
            (PaymentIntentStatus::Closed, "closed"),
            (PaymentIntentStatus::RefundRequired, "refund_required"),
        ];
        for (v, s) in intents {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
