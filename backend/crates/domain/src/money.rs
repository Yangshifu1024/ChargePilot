//! 资金域共享词表与纯规则：支付/退款状态、业务类型、人工复核校验与重放比对。
//! 语义基准：旧仓库 internal/domain/money/{payment,method,review,refund}.go，逐条对齐。
//!
//! 词表纪律：`BizType`（charge/wallet_recharge）与钱包流水的
//! `TxnBizType`（recharge/pay/refund/gift/freeze/unfreeze/adjust）是两组不同的词表，
//! 禁止取并集。各词表的「判定集合」（如 RefundablePaymentStatuses）是业务规则，
//! 不是全量取值，使用处必须引用具名常量。

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

// ---------- 词表枚举范式（二期票 06） ----------
//
// DB ENUM 字面量即 serde/sqlx 输出，由 [`wordlist_enum!`] 统一生成：
// `as_str` 是字面量锚点，serde 靠 `rename_all = "snake_case"` 与之同源，
// 并由 `db_literals_match_serde` 测试逐字节锁定。
//
// sqlx 三件套为什么手写（打样结论，2026-11-14 真库验证）：sqlx 0.8.6 的
// `#[derive(sqlx::Type)]` 强枚举只生成 `type_info() = MySqlTypeInfo::__enum()`
// （type=String + ENUM 标志，该形态是为参数绑定设计的），不覆写 `compatible`，
// 于是解码走默认的严格类型相等——而 MySQL 结果集把 ENUM 列报告为
// `ColumnType::Enum`，两者永不相等，连 `CAST AS CHAR`（VarString）也被拒。
// 因此兼容判定委托 `<str>::compatible` 的宽松白名单（接受 Enum 列且非 BINARY），
// 解码按字面量 parse，未知值即 schema 漂移报警。

/// DB 字面量不属于词表（schema 漂移或脏数据）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("未知字面量 `{value}`（词表 {wordlist}）")]
pub struct UnknownLiteral {
    /// 词表（Rust 类型名）。
    pub wordlist: &'static str,
    /// 实际读到的字面量。
    pub value: String,
}

use crate::wordlist::wordlist_enum;

// ---------- payment_order 词表（旧版 payment.go） ----------

wordlist_enum!(
    /// 支付与退款关联的业务类型：`payment_order.biz_type` 与 `refund_record.biz_type`
    /// 共享同一词表（一型三用，`invoice_request.biz_type` 同词表）。
    BizType: {
        Charge = "charge",
        WalletRecharge = "wallet_recharge",
    }
);

wordlist_enum!(
    /// payment_order.status 列的支付单业务状态（六值）。
    /// 注意与订单侧 `charge_order.payment_status` 区分：后者由金额推导得出。
    PaymentOrderStatus: {
        Initiated = "initiated",
        Paid = "paid",
        Closed = "closed",
        Refunded = "refunded",
        PartialRefunded = "partial_refunded",
        Failed = "failed",
    }
);

impl PaymentOrderStatus {
    /// 到账或已退回的判定集合（订单侧支付状态推导按此判定）。
    #[must_use]
    pub const fn is_paid_like(self) -> bool {
        matches!(self, Self::Paid | Self::Refunded | Self::PartialRefunded)
    }

    /// 仍可发起渠道退款的支付单状态集合（比 paid_like 少 refunded）。
    #[must_use]
    pub const fn is_refundable(self) -> bool {
        matches!(self, Self::Paid | Self::PartialRefunded)
    }
}

wordlist_enum!(
    /// refund_record.status 列的退款单状态（五值）。
    RefundStatus: {
        Pending = "pending",
        Processing = "processing",
        Success = "success",
        Failed = "failed",
        Rejected = "rejected",
    }
);

impl RefundStatus {
    /// 在途退款占用的取值集合：可退额度校验须扣除这些记录，
    /// 否则同一笔款项会被重复预占（旧版 UnsettledRefundStatuses）。
    #[must_use]
    pub const fn is_unsettled(self) -> bool {
        matches!(self, Self::Pending | Self::Processing)
    }
}

// ---------- method 词表（旧版 method.go） ----------

wordlist_enum!(
    /// payment_order.pay_method 列的支付方式。
    PayMethod: {
        Wechat = "wechat",
        Balance = "balance",
        Mixed = "mixed",
        Coupon = "coupon",
    }
);

/// charge_payment_intent.status 列的支付意图状态（五值）。
/// 与 PaymentOrderStatus 分立：共享 initiated/paid 字面量但含义不同，不得互相赋值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentIntentStatus(pub String);

impl PaymentIntentStatus {
    pub const INITIATED: &'static str = "initiated";
    pub const PAID: &'static str = "paid";
    pub const EXPIRED: &'static str = "expired";
    pub const CLOSED: &'static str = "closed";
    pub const REFUND_REQUIRED: &'static str = "refund_required";

    pub fn known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::INITIATED | Self::PAID | Self::EXPIRED | Self::CLOSED | Self::REFUND_REQUIRED
        )
    }
}

wordlist_enum!(
    /// refund_record.execution_policy 列的退款执行策略。
    ExecutionPolicy: {
        ManualReview = "manual_review",
        Automatic = "automatic",
    }
);

wordlist_enum!(
    /// charge_bill.status 列的账单状态。
    BillStatus: {
        Issued = "issued",
        Settled = "settled",
        Voided = "voided",
    }
);

wordlist_enum!(
    /// charge_order.payment_status 列的订单支付轴（四值；与
    /// [`PaymentOrderStatus`] 分立：仅随资金实际到账/退回推进，由金额推导）。
    ChargePaymentStatus: {
        Pending = "pending",
        Paid = "paid",
        Refunded = "refunded",
        PartialRefunded = "partial_refunded",
    }
);

// ---------- 人工复核共用规则（旧版 review.go） ----------

/// 复核重放与首次记录不一致（旧版 ErrReviewConflict）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("复核重放与首次记录不一致")]
pub struct ReviewConflict;

/// 复核依据的字数上限。
pub const MAX_REVIEW_COMMENT: usize = 255;

/// 校验人工复核依据：非空、不超过 255 字，且不含控制字符。
pub fn valid_review_comment(comment: &str) -> bool {
    if comment.trim().is_empty() || comment.chars().count() > MAX_REVIEW_COMMENT {
        return false;
    }
    !comment.chars().any(char::is_control)
}

/// 某次人工复核的首次记录（旧版 PriorReview）。
#[derive(Debug, Clone)]
pub struct PriorReview {
    pub actor_id: u64,
    pub approved: bool,
    pub comment: String,
}

/// 比对审核人、结论与依据；全部一致才沿用首次回执（旧版 ReviewReplay）。
pub fn review_replay(
    prior: &PriorReview,
    actor_id: u64,
    approved: bool,
    comment: &str,
) -> Result<(), ReviewConflict> {
    if prior.actor_id != actor_id || prior.approved != approved || prior.comment != comment {
        return Err(ReviewConflict);
    }
    Ok(())
}

/// 比对解冻操作的审核人与依据；解冻没有通过或驳回结论（旧版 ReleaseReplay）。
pub fn release_replay(
    prior: &PriorReview,
    actor_id: u64,
    comment: &str,
) -> Result<(), ReviewConflict> {
    if prior.actor_id != actor_id || prior.comment != comment {
        return Err(ReviewConflict);
    }
    Ok(())
}

// ---------- 退款事务内输入（旧版 refund.go） ----------

/// 同库事务内的退款业务输入，不携带数据库模型或渠道完成声明（旧版 RefundDraft）。
#[derive(Debug, Clone)]
pub struct RefundDraft {
    /// 退款单号，重试不得重新生成。
    pub refund_no: String,
    /// 支付主键，调用方已持有支付行锁。
    pub payment_order_id: u64,
    /// 支付所属用户。
    pub user_id: u64,
    /// charge 或 wallet_recharge。
    pub biz_type: BizType,
    /// 业务编号；迟到支付尚未建单时为 0。
    pub biz_id: u64,
    /// 本笔待退金额，单位分，必须大于 0。
    pub amount_cents: i64,
    /// 持久化退款原因，不含凭据或敏感数据。
    pub reason: String,
    /// 首次申请月份的 UTC 月首；重放沿用月份。
    pub created_month: chrono::NaiveDate,
    /// 首次可执行的 UTC 时刻；None 使用数据库默认值。
    pub next_attempt_at: Option<DateTime<Utc>>,
}

impl RefundDraft {
    /// 从当前时刻派生 created_month（UTC 月首，旧版 time.Date(y, m, 1, ...) 语义）。
    pub fn month_start_of(now: DateTime<Utc>) -> chrono::NaiveDate {
        now.date_naive()
            .with_day(1)
            .unwrap_or_else(|| now.date_naive())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_comment_validation() {
        assert!(valid_review_comment("正常依据"));
        assert!(!valid_review_comment("   "));
        assert!(!valid_review_comment(&"长".repeat(256)));
        assert!(!valid_review_comment("含控制\u{7}字符"));
    }

    #[test]
    fn replay_rules() {
        let prior = PriorReview {
            actor_id: 1,
            approved: true,
            comment: "ok".into(),
        };
        assert!(review_replay(&prior, 1, true, "ok").is_ok());
        assert!(review_replay(&prior, 2, true, "ok").is_err());
        assert!(release_replay(&prior, 1, "ok").is_ok());
        assert!(release_replay(&prior, 1, "changed").is_err());
    }

    #[test]
    fn status_sets() {
        assert!(PaymentOrderStatus::Paid.is_paid_like());
        assert!(!PaymentOrderStatus::Initiated.is_refundable());
        assert!(RefundStatus::Pending.is_unsettled());
        assert!(!RefundStatus::Failed.is_unsettled());
    }

    /// 字面量对照（票 06 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致，
    /// 同时锁 serde 与 sqlx 两套 rename_all 的同源。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let payment = [
            (PaymentOrderStatus::Initiated, "initiated"),
            (PaymentOrderStatus::Paid, "paid"),
            (PaymentOrderStatus::Closed, "closed"),
            (PaymentOrderStatus::Refunded, "refunded"),
            (PaymentOrderStatus::PartialRefunded, "partial_refunded"),
            (PaymentOrderStatus::Failed, "failed"),
        ];
        for (v, s) in payment {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let charge_axis = [
            (ChargePaymentStatus::Pending, "pending"),
            (ChargePaymentStatus::Paid, "paid"),
            (ChargePaymentStatus::Refunded, "refunded"),
            (ChargePaymentStatus::PartialRefunded, "partial_refunded"),
        ];
        for (v, s) in charge_axis {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let biz = [
            (BizType::Charge, "charge"),
            (BizType::WalletRecharge, "wallet_recharge"),
        ];
        for (v, s) in biz {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let refund = [
            (RefundStatus::Pending, "pending"),
            (RefundStatus::Processing, "processing"),
            (RefundStatus::Success, "success"),
            (RefundStatus::Failed, "failed"),
            (RefundStatus::Rejected, "rejected"),
        ];
        for (v, s) in refund {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let policy = [
            (ExecutionPolicy::ManualReview, "manual_review"),
            (ExecutionPolicy::Automatic, "automatic"),
        ];
        for (v, s) in policy {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let bill = [
            (BillStatus::Issued, "issued"),
            (BillStatus::Settled, "settled"),
            (BillStatus::Voided, "voided"),
        ];
        for (v, s) in bill {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let method = [
            (PayMethod::Wechat, "wechat"),
            (PayMethod::Balance, "balance"),
            (PayMethod::Mixed, "mixed"),
            (PayMethod::Coupon, "coupon"),
        ];
        for (v, s) in method {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
