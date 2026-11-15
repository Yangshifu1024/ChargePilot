//! 在线卡词表与刷卡准入纯规则（旧版 domain/card 与 card.CanExtend 的单体对应物）。
//! 语义基准：旧仓库 internal/domain/card/*.go 与 internal/card/extension.go，逐行对齐。

use crate::charging::scheme::Package;
use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// online_card.status 列的在线卡状态（四值）。
    CardStatus: {
        Active = "active",
        Lost = "lost",
        Disabled = "disabled",
        Unbound = "unbound",
    }
);

wordlist_enum!(
    /// card_operation.kind 列的刷卡操作种类（两值）。
    CardOperationKind: {
        Start = "start",
        Extend = "extend",
    }
);

wordlist_enum!(
    /// card_operation.status 列的刷卡操作状态（三值）。
    CardOperationStatus: {
        Confirming = "confirming",
        Confirmed = "confirmed",
        Failed = "failed",
    }
);

wordlist_enum!(
    /// card_event_delivery.status 列的卡事件投递状态（两值）。
    /// 该表当前无 Rust 写点（事件投递接线后的落点，票 10 范围内先建型）。
    CardDeliveryStatus: {
        Pending = "pending",
        Done = "done",
    }
);

/// 判断同卡是否可购买正价时长套餐（旧版 CanExtend）。
/// 首购传 `card == order_card` 恒真、`purchased = 0`；续购传会话的卡与累计值。
/// 规则：同卡、时长套餐、价格与时长为正、无待确认扣款、可用余额足够，
/// 且累计购买不超过会话冻结的时长上限。
pub fn can_extend(
    card: u64,
    order_card: u64,
    package: &Package,
    purchased_minutes: u16,
    max_minutes: u16,
    available_balance_cents: i64,
    pending_operation: bool,
) -> bool {
    card == order_card
        && package.mode == "duration"
        && package.price_cents > 0
        && package.minutes > 0
        && !pending_operation
        && available_balance_cents >= package.price_cents
        && purchased_minutes as u32 + package.minutes as u32 <= max_minutes as u32
}

/// 卡号规范校验（旧版 applyInTx 内联校验）：非空十进制、32 位协议范围、
/// 规范文本（去零后回写一致）。
pub fn valid_card_no(card_no: &str) -> bool {
    if card_no.is_empty() || card_no.len() > 64 {
        return false;
    }
    match card_no.parse::<u32>() {
        Ok(n) => n != 0 && n.to_string() == card_no,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duration_package(price: i64, minutes: u64) -> Package {
        Package {
            id: 1,
            name: "时长卡".into(),
            mode: "duration".into(),
            price_cents: price,
            minutes,
            kwh: 0,
        }
    }

    #[test]
    fn can_extend_same_card_and_limits() {
        let p = duration_package(100, 60);
        // 同卡、余额足够、未超上限 → 可续。
        assert!(can_extend(7, 7, &p, 60, 600, 500, false));
        // 异卡拒绝。
        assert!(!can_extend(7, 8, &p, 60, 600, 500, false));
        // 累计超上限拒绝（60+60 > 100）。
        assert!(!can_extend(7, 7, &p, 60, 100, 500, false));
        // 恰好到上限允许（60+60 <= 120）。
        assert!(can_extend(7, 7, &p, 60, 120, 500, false));
        // 有待确认扣款拒绝。
        assert!(!can_extend(7, 7, &p, 60, 600, 500, true));
        // 余额不足拒绝。
        assert!(!can_extend(7, 7, &p, 60, 600, 99, false));
    }

    #[test]
    fn can_extend_requires_positive_duration_package() {
        // 非时长套餐拒绝。
        let amount = Package {
            mode: "amount".into(),
            ..duration_package(100, 60)
        };
        assert!(!can_extend(7, 7, &amount, 0, 600, 500, false));
        // 零价/零时长拒绝。
        assert!(!can_extend(
            7,
            7,
            &duration_package(0, 60),
            0,
            600,
            500,
            false
        ));
        assert!(!can_extend(
            7,
            7,
            &duration_package(100, 0),
            0,
            600,
            500,
            false
        ));
    }

    #[test]
    fn valid_card_no_rules() {
        assert!(valid_card_no("12345678"));
        assert!(!valid_card_no("0"));
        assert!(!valid_card_no("0123")); // 规范十进制：前导零拒绝
        assert!(!valid_card_no(""));
        assert!(!valid_card_no("4294967296")); // 超出 32 位协议范围
        assert!(!valid_card_no("abc"));
        assert!(!valid_card_no(&"9".repeat(65)));
    }
}

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照（票 10 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let card = [
            (CardStatus::Active, "active"),
            (CardStatus::Lost, "lost"),
            (CardStatus::Disabled, "disabled"),
            (CardStatus::Unbound, "unbound"),
        ];
        for (v, s) in card {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let kind = [
            (CardOperationKind::Start, "start"),
            (CardOperationKind::Extend, "extend"),
        ];
        for (v, s) in kind {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let op = [
            (CardOperationStatus::Confirming, "confirming"),
            (CardOperationStatus::Confirmed, "confirmed"),
            (CardOperationStatus::Failed, "failed"),
        ];
        for (v, s) in op {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let delivery = [
            (CardDeliveryStatus::Pending, "pending"),
            (CardDeliveryStatus::Done, "done"),
        ];
        for (v, s) in delivery {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
