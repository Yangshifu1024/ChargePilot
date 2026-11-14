//! 订单域共享词表（票 07）：充电订单主状态与用户反馈两词表。
//!
//! 与 [`crate::charging_state`] 的关系（票 07 裁决，防误判）：状态机是
//! 每态新类型 + 迁移方法的**进程内证据**机制，词表枚举是**数据库往返**
//! 机制，两者并存；证据类型的接线归票 14，本票不消费。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// charge_order.status 列的充电订单主状态（八值，迁移图见
    /// `docs/design/state-machine.md`）。
    ChargeOrderStatus: {
        PendingPayment = "pending_payment",
        Paid = "paid",
        Charging = "charging",
        Completed = "completed",
        Cancelled = "cancelled",
        Failed = "failed",
        Refunding = "refunding",
        Refunded = "refunded",
    }
);

impl ChargeOrderStatus {
    /// 占用端口的订单状态：已支付未起充与充电中（旧版 paidOrChargingOrderStatuses）。
    #[must_use]
    pub const fn is_ongoing(self) -> bool {
        matches!(self, Self::Paid | Self::Charging)
    }
}

wordlist_enum!(
    /// feedback.category 列的反馈分类（三值；与评分 rating 同名值但含义不同——
    /// 该列语义是「分类标签」，评分本身存 rating 列）。
    FeedbackCategory: {
        Rating = "rating",
        Complaint = "complaint",
        Suggestion = "suggestion",
    }
);

wordlist_enum!(
    /// feedback.status 列的反馈工单状态（三值）。
    FeedbackStatus: {
        Pending = "pending",
        Processed = "processed",
        Closed = "closed",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    /// 字面量对照（票 07 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let order = [
            (ChargeOrderStatus::PendingPayment, "pending_payment"),
            (ChargeOrderStatus::Paid, "paid"),
            (ChargeOrderStatus::Charging, "charging"),
            (ChargeOrderStatus::Completed, "completed"),
            (ChargeOrderStatus::Cancelled, "cancelled"),
            (ChargeOrderStatus::Failed, "failed"),
            (ChargeOrderStatus::Refunding, "refunding"),
            (ChargeOrderStatus::Refunded, "refunded"),
        ];
        for (v, s) in order {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let category = [
            (FeedbackCategory::Rating, "rating"),
            (FeedbackCategory::Complaint, "complaint"),
            (FeedbackCategory::Suggestion, "suggestion"),
        ];
        for (v, s) in category {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let status = [
            (FeedbackStatus::Pending, "pending"),
            (FeedbackStatus::Processed, "processed"),
            (FeedbackStatus::Closed, "closed"),
        ];
        for (v, s) in status {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }

    #[test]
    fn ongoing_set() {
        assert!(ChargeOrderStatus::Paid.is_ongoing());
        assert!(ChargeOrderStatus::Charging.is_ongoing());
        assert!(!ChargeOrderStatus::Completed.is_ongoing());
        assert!(!ChargeOrderStatus::PendingPayment.is_ongoing());
    }
}
