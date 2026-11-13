//! 资金域共享词表与纯规则：支付/退款状态、业务类型、人工复核校验与重放比对。
//! 语义基准：旧仓库 internal/domain/money/{payment,method,review,refund}.go，逐条对齐。
//!
//! 词表纪律：`PaymentBizType`（charge/wallet_recharge）与钱包流水的
//! `TxnBizType`（recharge/pay/refund/gift/freeze/unfreeze/adjust）是两组不同的词表，
//! 禁止取并集。各词表的「判定集合」（如 RefundablePaymentStatuses）是业务规则，
//! 不是全量取值，使用处必须引用具名常量。

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

// ---------- payment_order 词表（旧版 payment.go） ----------

/// 支付与退款关联的业务类型：`payment_order.biz_type` 与 `refund_record.biz_type`
/// 共享同一词表（charge / wallet_recharge）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentBizType(pub String);

impl PaymentBizType {
    pub const CHARGE: &'static str = "charge";
    pub const WALLET_RECHARGE: &'static str = "wallet_recharge";

    pub fn valid(&self) -> bool {
        matches!(self.0.as_str(), Self::CHARGE | Self::WALLET_RECHARGE)
    }
}

/// payment_order.status 列的支付单业务状态（六值）。
/// 注意与订单侧 charge_order.payment_status 区分：后者由金额推导得出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentStatus(pub String);

impl PaymentStatus {
    pub const INITIATED: &'static str = "initiated";
    pub const PAID: &'static str = "paid";
    pub const CLOSED: &'static str = "closed";
    pub const REFUNDED: &'static str = "refunded";
    pub const PARTIAL_REFUNDED: &'static str = "partial_refunded";
    pub const FAILED: &'static str = "failed";

    /// 全量取值词表（读取路径遇到未知值保持原值透传）。
    pub fn valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::INITIATED
                | Self::PAID
                | Self::CLOSED
                | Self::REFUNDED
                | Self::PARTIAL_REFUNDED
                | Self::FAILED
        )
    }

    /// 到账或已退回的判定集合（订单侧支付状态推导按此判定）。
    pub fn is_paid_like(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::PAID | Self::REFUNDED | Self::PARTIAL_REFUNDED
        )
    }

    /// 仍可发起渠道退款的支付单状态集合（比 paid_like 少 refunded）。
    pub fn is_refundable(&self) -> bool {
        matches!(self.0.as_str(), Self::PAID | Self::PARTIAL_REFUNDED)
    }
}

/// refund_record.status 列的退款单状态（五值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundStatus(pub String);

impl RefundStatus {
    pub const PENDING: &'static str = "pending";
    pub const PROCESSING: &'static str = "processing";
    pub const SUCCESS: &'static str = "success";
    pub const FAILED: &'static str = "failed";
    pub const REJECTED: &'static str = "rejected";

    pub fn valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::PENDING | Self::PROCESSING | Self::SUCCESS | Self::FAILED | Self::REJECTED
        )
    }

    /// 在途退款占用的取值集合：可退额度校验须扣除这些记录，
    /// 否则同一笔款项会被重复预占（旧版 UnsettledRefundStatuses）。
    pub fn is_unsettled(&self) -> bool {
        matches!(self.0.as_str(), Self::PENDING | Self::PROCESSING)
    }
}

// ---------- method 词表（旧版 method.go） ----------

/// payment_order.pay_method 列的支付方式（wechat/balance/mixed/coupon）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayMethod(pub String);

impl PayMethod {
    pub const WECHAT: &'static str = "wechat";
    pub const BALANCE: &'static str = "balance";
    pub const MIXED: &'static str = "mixed";
    pub const COUPON: &'static str = "coupon";

    pub fn known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::WECHAT | Self::BALANCE | Self::MIXED | Self::COUPON
        )
    }
}

/// charge_payment_intent.status 列的支付意图状态（五值）。
/// 与 PaymentStatus 分立：共享 initiated/paid 字面量但含义不同，不得互相赋值。
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

/// refund_record.execution_policy 列的退款执行策略（manual_review/automatic）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy(pub String);

impl ExecutionPolicy {
    pub const MANUAL_REVIEW: &'static str = "manual_review";
    pub const AUTOMATIC: &'static str = "automatic";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::MANUAL_REVIEW | Self::AUTOMATIC)
    }
}

/// charge_bill.status 列的账单状态（issued/settled/voided）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillStatus(pub String);

impl BillStatus {
    pub const ISSUED: &'static str = "issued";
    pub const SETTLED: &'static str = "settled";
    pub const VOIDED: &'static str = "voided";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::ISSUED | Self::SETTLED | Self::VOIDED)
    }
}

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
    pub biz_type: PaymentBizType,
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
        assert!(PaymentStatus("paid".into()).is_paid_like());
        assert!(!PaymentStatus("initiated".into()).is_refundable());
        assert!(RefundStatus("pending".into()).is_unsettled());
        assert!(!RefundStatus("failed".into()).is_unsettled());
    }
}
