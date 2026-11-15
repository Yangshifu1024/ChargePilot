//! 资金域共享词表与纯规则：支付/退款状态、业务类型、人工复核校验、重放比对与退款事务输入。
//!
//! 词表纪律：`BizType`（charge/wallet_recharge）与钱包流水的
//! `TxnBizType`（recharge/pay/refund/gift/freeze/unfreeze/adjust）是两组不同的词表，
//! 禁止取并集。各词表的「判定集合」（如 RefundablePaymentStatuses）是业务规则，
//! 不是全量取值，使用处必须引用具名常量。

pub mod refund;
pub mod review;
pub mod status;

pub use refund::RefundDraft;
pub use review::{
    MAX_REVIEW_COMMENT, PriorReview, ReviewConflict, release_replay, review_replay,
    valid_review_comment,
};
pub use status::{
    BillStatus, BizType, ChargePaymentStatus, ExecutionPolicy, PayMethod, PaymentOrderStatus,
    RefundStatus, UnknownLiteral,
};
