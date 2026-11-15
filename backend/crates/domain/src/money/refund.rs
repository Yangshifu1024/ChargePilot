//! 同库事务内的退款业务输入：不携带数据库模型或渠道完成声明。

use chrono::{DateTime, Datelike, Utc};

use super::status::BizType;

/// 同库事务内的退款业务输入，不携带数据库模型或渠道完成声明。
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
    /// 从当前时刻派生 created_month（UTC 月首）。
    pub fn month_start_of(now: DateTime<Utc>) -> chrono::NaiveDate {
        now.date_naive()
            .with_day(1)
            .unwrap_or_else(|| now.date_naive())
    }
}
