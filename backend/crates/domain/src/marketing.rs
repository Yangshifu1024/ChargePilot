//! 营销词表与优惠券纯规则（旧版 marketing/status.go、rules_coupon.go、
//! rules_coupon_discount.go 的单体对应物）。
//! 语义基准：旧仓库逐行对齐；优惠计算以分单位精确（decimal 中间计算、
//! 应付金额四舍五入到分），与旧版差分对照验收（tests/golden/discount.json）。

use chrono::{DateTime, Utc};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// coupon.status 列的券模板启停状态（active/disabled）。
/// 与 ActivityStatus 取值串相同但分属 coupon 与 coupon_activity_rule 两表，
/// 语义不同，不得合并；与 GrantStatus 也不是同一词表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouponStatus(pub String);

impl CouponStatus {
    pub const ACTIVE: &'static str = "active";
    pub const DISABLED: &'static str = "disabled";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::ACTIVE | Self::DISABLED)
    }
}

/// coupon_activity_rule.status 列的活动规则启停状态（active/disabled）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityStatus(pub String);

impl ActivityStatus {
    pub const ACTIVE: &'static str = "active";
    pub const DISABLED: &'static str = "disabled";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::ACTIVE | Self::DISABLED)
    }
}

/// coupon.discount_type 列的优惠计算类型（amount/percentage/time_free）。
/// 三种类型的优惠金额列互斥：amount 读 discount_value_cents、
/// percentage 读 discount_percent、time_free 读 free_minutes。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscountType(pub String);

impl DiscountType {
    pub const AMOUNT: &'static str = "amount";
    pub const PERCENTAGE: &'static str = "percentage";
    pub const TIME_FREE: &'static str = "time_free";

    pub fn known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::AMOUNT | Self::PERCENTAGE | Self::TIME_FREE
        )
    }
}

/// coupon_activity_rule.trigger_type 列的活动触发类型
/// （first_recharge/invite_reward/threshold_redeem/holiday）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerType(pub String);

impl TriggerType {
    pub const FIRST_RECHARGE: &'static str = "first_recharge";
    pub const INVITE_REWARD: &'static str = "invite_reward";
    pub const THRESHOLD_REDEEM: &'static str = "threshold_redeem";
    pub const HOLIDAY: &'static str = "holiday";

    pub fn known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::FIRST_RECHARGE | Self::INVITE_REWARD | Self::THRESHOLD_REDEEM | Self::HOLIDAY
        )
    }
}

/// coupon_grant.status 列的券状态（unused/used/expired）。
/// 与 coupon.status（模板启停）词表不同，不得互换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantStatus(pub String);

impl GrantStatus {
    pub const UNUSED: &'static str = "unused";
    pub const USED: &'static str = "used";
    pub const EXPIRED: &'static str = "expired";

    pub fn valid(&self) -> bool {
        matches!(self.0.as_str(), Self::UNUSED | Self::USED | Self::EXPIRED)
    }
}

/// coupon_grant.grant_source 列的发放来源
/// （register/activity/invite/manual/invite_reward）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSource(pub String);

impl GrantSource {
    pub const REGISTER: &'static str = "register";
    pub const ACTIVITY: &'static str = "activity";
    pub const INVITE: &'static str = "invite";
    pub const MANUAL: &'static str = "manual";
    pub const INVITE_REWARD: &'static str = "invite_reward";

    pub fn valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::REGISTER | Self::ACTIVITY | Self::INVITE | Self::MANUAL | Self::INVITE_REWARD
        )
    }
}

/// 优惠计算输入：发放券关联的模板规则（旧版 CouponGrant 的优惠字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantRule {
    /// 优惠类型（DiscountType）。
    pub discount_type: String,
    /// 使用门槛，分。
    pub min_charge_cents: i64,
    /// amount 型减免分值；其他类型为 None。
    pub discount_value_cents: Option<i64>,
    /// percentage 型应付比例（80 表示八折）；其他类型为 None。
    pub discount_percent: Option<f64>,
    /// time_free 型免费分钟。
    pub free_minutes: u32,
}

/// 优惠计算错误（旧版 ErrCouponThreshold / ErrCouponNotFound 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscountError {
    /// 未达到使用门槛或订单金额非正。
    Threshold,
    /// 优惠参数缺失或非法、类型未知。
    NotFound,
}

impl std::fmt::Display for DiscountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscountError::Threshold => f.write_str("未达到该优惠券的使用门槛"),
            DiscountError::NotFound => f.write_str("优惠券不存在或未发放给该账号"),
        }
    }
}

impl std::error::Error for DiscountError {}

/// 时长券的固定服务费率（分/分钟）；旧版常量 0——时长券尚未按套餐服务费折算。
const SERVICE_RATE_PER_MINUTE: i64 = 0;

/// 按原价计算优惠（旧版 discountFor），单位分。
/// 减免最多为原价；折扣先算应付额再相减；时长券优惠恒为零（固定零费率）。
pub fn discount_for(grant: &GrantRule, total_cents: i64) -> Result<i64, DiscountError> {
    if total_cents <= 0 || grant.min_charge_cents > total_cents {
        return Err(DiscountError::Threshold);
    }
    let discount = match grant.discount_type.as_str() {
        DiscountType::AMOUNT => {
            let Some(amount) = grant.discount_value_cents else {
                return Err(DiscountError::NotFound);
            };
            if amount <= 0 {
                return Err(DiscountError::NotFound);
            }
            amount
        }
        DiscountType::PERCENTAGE => {
            let Some(percent) = grant.discount_percent else {
                return Err(DiscountError::NotFound);
            };
            if !(percent > 0.0 && percent < 100.0) {
                return Err(DiscountError::NotFound);
            }
            // 原价乘应付比例，用十进制计算；应付金额四舍五入到分，再从原价扣除。
            let total = Decimal::from(total_cents);
            let percent_dec = Decimal::from_f64(percent).ok_or(DiscountError::NotFound)?;
            let keep = (total * percent_dec / Decimal::from(100))
                .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
                .to_i64();
            let Some(keep) = keep else {
                return Err(DiscountError::NotFound);
            };
            total_cents - keep
        }
        DiscountType::TIME_FREE => {
            if grant.free_minutes == 0 {
                return Err(DiscountError::NotFound);
            }
            grant.free_minutes as i64 * 60 * SERVICE_RATE_PER_MINUTE
        }
        _ => return Err(DiscountError::NotFound),
    };
    Ok(discount.clamp(0, total_cents))
}

/// `Decimal` 取整到 i64 的内部辅助（四舍五入失败视为参数非法）。
trait ToI64Round {
    fn to_i64(self) -> Option<i64>;
}
impl ToI64Round for Decimal {
    fn to_i64(self) -> Option<i64> {
        use rust_decimal::prelude::ToPrimitive;
        rust_decimal::prelude::ToPrimitive::to_i64(&self)
    }
}

/// 发放准入需要的模板字段（旧版 CouponTemplate）。
#[derive(Debug, Clone, PartialEq)]
pub struct CouponTemplate {
    pub template_id: u64,
    /// 模板状态（CouponStatus）；仅 active 可发放。
    pub status: String,
    /// 发放总量上限，0 表示不限量。
    pub total_quota: u32,
    /// 每人发放上限，必须大于 0。
    pub per_user_quota: u32,
    /// 领取后的有效小时数。
    pub valid_hours: u32,
    /// 生效开始；None 立即生效。
    pub start_at: Option<DateTime<Utc>>,
    /// 生效结束；None 长期有效。
    pub end_at: Option<DateTime<Utc>>,
}

/// 某券模板当前的发放量（旧版 CouponUsage）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CouponUsage {
    pub total_granted: i64,
    pub user_granted: i64,
}

/// 检查模板启用、时间有效且总量/单人份数未达上限（旧版 CheckGrantable）。
pub fn check_grantable(
    now: DateTime<Utc>,
    coupon: &CouponTemplate,
    usage: CouponUsage,
) -> Result<(), DiscountError> {
    if coupon.status != CouponStatus::ACTIVE {
        return Err(DiscountError::NotFound);
    }
    if coupon.start_at.is_some_and(|t| now < t) {
        return Err(DiscountError::NotFound);
    }
    if coupon.end_at.is_some_and(|t| now >= t) {
        return Err(DiscountError::NotFound);
    }
    if coupon.total_quota > 0 && usage.total_granted >= coupon.total_quota as i64 {
        return Err(DiscountError::NotFound);
    }
    if usage.user_granted >= coupon.per_user_quota as i64 {
        return Err(DiscountError::NotFound);
    }
    Ok(())
}

/// 从发券时间起计算有效小时；模板结束更早时以模板结束为上限（旧版 GrantExpiry）。
pub fn grant_expiry(now: DateTime<Utc>, coupon: &CouponTemplate) -> DateTime<Utc> {
    let expires = now + chrono::Duration::hours(coupon.valid_hours as i64);
    match coupon.end_at {
        Some(end) if end < expires => end,
        _ => expires,
    }
}

/// 发放重放回执（旧版 CouponGrantReceipt）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantReceipt {
    pub request_id: String,
    pub coupon_id: u64,
    pub user_id: u64,
    pub coupon_grant_id: u64,
}

/// 同请求号且券、用户一致返回首次发放 ID；不一致返回冲突（旧版 ReplayGrant）。
pub fn replay_grant(
    receipt: &GrantReceipt,
    request_id: &str,
    coupon_id: u64,
    user_id: u64,
) -> Result<u64, DiscountError> {
    if receipt.request_id != request_id
        || receipt.coupon_id != coupon_id
        || receipt.user_id != user_id
    {
        return Err(DiscountError::NotFound);
    }
    Ok(receipt.coupon_grant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: &str, min: i64, value: Option<i64>, percent: Option<f64>, free: u32) -> GrantRule {
        GrantRule {
            discount_type: kind.into(),
            min_charge_cents: min,
            discount_value_cents: value,
            discount_percent: percent,
            free_minutes: free,
        }
    }

    #[test]
    fn amount_discount_basics() {
        assert_eq!(
            discount_for(&rule("amount", 100, Some(50), None, 0), 99),
            Err(DiscountError::Threshold)
        );
        assert_eq!(discount_for(&rule("amount", 100, Some(50), None, 0), 100).unwrap(), 50);
        assert_eq!(discount_for(&rule("amount", 0, Some(50), None, 0), 100).unwrap(), 50);
        assert_eq!(discount_for(&rule("amount", 0, Some(120), None, 0), 100).unwrap(), 100, "减免封顶为原价");
        assert!(discount_for(&rule("amount", 0, Some(0), None, 0), 100).is_err());
        assert!(discount_for(&rule("amount", 0, None, None, 0), 100).is_err());
    }

    #[test]
    fn percentage_discount_rounding() {
        // 50% → 应付 50 → 优惠 50。
        assert_eq!(discount_for(&rule("percentage", 0, None, Some(50.0), 0), 100).unwrap(), 50);
        // 50.5% → 应付 50.5 四舍五入 51 → 优惠 49（half away from zero）。
        assert_eq!(discount_for(&rule("percentage", 0, None, Some(50.5), 0), 100).unwrap(), 49);
        // 33.33% of 1000 → 应付 333.3 → 333 → 优惠 667。
        assert_eq!(discount_for(&rule("percentage", 0, None, Some(33.33), 0), 1000).unwrap(), 667);
        // 0.01% of 99 → 应付 0.0099 → 0 → 优惠 99。
        assert_eq!(discount_for(&rule("percentage", 0, None, Some(0.01), 0), 99).unwrap(), 99);
        assert!(discount_for(&rule("percentage", 0, None, Some(0.0), 0), 100).is_err());
        assert!(discount_for(&rule("percentage", 0, None, Some(100.0), 0), 100).is_err());
        assert!(discount_for(&rule("percentage", 0, None, Some(150.0), 0), 100).is_err());
    }

    #[test]
    fn time_free_and_unknown() {
        // 时长券固定零费率：优惠恒 0（仍要过门槛）。
        assert_eq!(discount_for(&rule("time_free", 10, None, None, 30), 100).unwrap(), 0);
        assert!(discount_for(&rule("time_free", 10, None, None, 0), 100).is_err());
        assert_eq!(discount_for(&rule("cashback", 0, Some(10), None, 0), 100), Err(DiscountError::NotFound));
        assert_eq!(discount_for(&rule("time_free", 10, None, None, 30), 9), Err(DiscountError::Threshold));
        assert_eq!(discount_for(&rule("amount", 0, Some(10), None, 0), 0), Err(DiscountError::Threshold));
    }

    #[test]
    fn grant_rules() {
        let tpl = CouponTemplate {
            template_id: 1,
            status: "active".into(),
            total_quota: 2,
            per_user_quota: 1,
            valid_hours: 24,
            start_at: None,
            end_at: None,
        };
        let now = Utc::now();
        assert!(check_grantable(now, &tpl, CouponUsage { total_granted: 1, user_granted: 0 }).is_ok());
        assert!(check_grantable(now, &tpl, CouponUsage { total_granted: 2, user_granted: 0 }).is_err());
        assert!(check_grantable(now, &tpl, CouponUsage { total_granted: 0, user_granted: 1 }).is_err());
        let disabled = CouponTemplate { status: "disabled".into(), ..tpl.clone() };
        assert!(check_grantable(now, &disabled, CouponUsage { total_granted: 0, user_granted: 0 }).is_err());
        // 到期 = now + 24h。
        assert_eq!(grant_expiry(now, &tpl), now + chrono::Duration::hours(24));
        // 模板结束早于有效时长 → 以结束为上限。
        let bounded = CouponTemplate { end_at: Some(now + chrono::Duration::hours(2)), ..tpl.clone() };
        assert_eq!(grant_expiry(now, &bounded), now + chrono::Duration::hours(2));
        // 重放：同请求同券同人复用；内容不同冲突。
        let receipt = GrantReceipt { request_id: "r1".into(), coupon_id: 1, user_id: 2, coupon_grant_id: 9 };
        assert_eq!(replay_grant(&receipt, "r1", 1, 2).unwrap(), 9);
        assert!(replay_grant(&receipt, "r2", 1, 2).is_err());
        assert!(replay_grant(&receipt, "r1", 3, 2).is_err());
    }
}
