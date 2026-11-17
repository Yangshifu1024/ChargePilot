//! 营销词表与优惠券、活动发放的纯规则（零 IO）。
//! 优惠计算以分为单位：中间量用 decimal，应付金额四舍五入到分；
//! 黄金样本见 `tests/golden/discount.json`。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::{Deserialize, Serialize};

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// coupon.status 列的券模板启停状态（两值）。与
    /// [`ActivityStatus`] 取值串相同但分属 coupon 与 coupon_activity_rule
    /// 两表，语义不同，不合并；与 [`GrantStatus`] 也不是同一词表。
    CouponStatus: {
        Active = "active",
        Disabled = "disabled",
    }
);

wordlist_enum!(
    /// coupon_activity_rule.status 列的活动规则启停状态（两值）。
    ActivityStatus: {
        Active = "active",
        Disabled = "disabled",
    }
);

wordlist_enum!(
    /// coupon.discount_type 列的优惠计算类型（三值）。三种类型的优惠金额
    /// 列互斥：amount 读 discount_value_cents、percentage 读 discount_percent、
    /// time_free 读 free_minutes；优惠计算按本枚举 match 穷尽分派。
    DiscountType: {
        Amount = "amount",
        Percentage = "percentage",
        TimeFree = "time_free",
    }
);

wordlist_enum!(
    /// coupon_activity_rule.trigger_type 列的活动触发类型（四值）。
    ActivityTriggerType: {
        FirstRecharge = "first_recharge",
        InviteReward = "invite_reward",
        ThresholdRedeem = "threshold_redeem",
        Holiday = "holiday",
    }
);

wordlist_enum!(
    /// coupon_grant.status 列的券状态（三值）。与 coupon.status（模板启停）
    /// 词表不同，不互换。
    GrantStatus: {
        Unused = "unused",
        Used = "used",
        Expired = "expired",
    }
);

wordlist_enum!(
    /// coupon_grant.grant_source 列的发放来源（五值）。
    GrantSource: {
        Register = "register",
        Activity = "activity",
        Invite = "invite",
        Manual = "manual",
        InviteReward = "invite_reward",
    }
);

/// 优惠计算输入：发放券关联的模板规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantRule {
    /// 优惠类型（词表枚举，优惠计算 match 穷尽分派）。
    pub discount_type: DiscountType,
    /// 使用门槛，分。
    pub min_charge_cents: i64,
    /// amount 型减免分值；其他类型为 None。
    pub discount_value_cents: Option<i64>,
    /// percentage 型应付比例（80 表示八折）；其他类型为 None。
    pub discount_percent: Option<f64>,
    /// time_free 型免费分钟。
    pub free_minutes: u32,
}

/// 优惠计算错误。
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

/// 时长券的固定服务费率（分/分钟）；常量 0——时长券尚未按套餐服务费折算。
const SERVICE_RATE_PER_MINUTE: i64 = 0;

/// 按原价计算优惠，单位分。
/// 减免最多为原价；折扣先算应付额再相减；时长券优惠恒为零（固定零费率）。
pub fn discount_for(grant: &GrantRule, total_cents: i64) -> Result<i64, DiscountError> {
    if total_cents <= 0 || grant.min_charge_cents > total_cents {
        return Err(DiscountError::Threshold);
    }
    let discount = match grant.discount_type {
        DiscountType::Amount => {
            let Some(amount) = grant.discount_value_cents else {
                return Err(DiscountError::NotFound);
            };
            if amount <= 0 {
                return Err(DiscountError::NotFound);
            }
            amount
        }
        DiscountType::Percentage => {
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
        DiscountType::TimeFree => {
            if grant.free_minutes == 0 {
                return Err(DiscountError::NotFound);
            }
            grant.free_minutes as i64 * 60 * SERVICE_RATE_PER_MINUTE
        }
    };
    Ok(discount.clamp(0, total_cents))
}

/// `Decimal` 取整到 i64 的内部辅助（四舍五入失败视为参数非法）。
trait ToI64Round {
    fn to_i64(self) -> Option<i64>;
}
impl ToI64Round for Decimal {
    fn to_i64(self) -> Option<i64> {
        rust_decimal::prelude::ToPrimitive::to_i64(&self)
    }
}

/// 发放准入需要的模板字段。
#[derive(Debug, Clone, PartialEq)]
pub struct CouponTemplate {
    pub template_id: u64,
    /// 模板状态（词表枚举）；仅 active 可发放。
    pub status: CouponStatus,
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

/// 某券模板当前的发放量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CouponUsage {
    pub total_granted: i64,
    pub user_granted: i64,
}

/// 检查模板启用、时间有效且总量/单人份数未达上限。
pub fn check_grantable(
    now: DateTime<Utc>,
    coupon: &CouponTemplate,
    usage: CouponUsage,
) -> Result<(), DiscountError> {
    if coupon.status != CouponStatus::Active {
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

/// 从发券时间起计算有效小时；模板结束更早时以模板结束为上限。
pub fn grant_expiry(now: DateTime<Utc>, coupon: &CouponTemplate) -> DateTime<Utc> {
    let expires = now + chrono::Duration::hours(coupon.valid_hours as i64);
    match coupon.end_at {
        Some(end) if end < expires => end,
        _ => expires,
    }
}

/// 发放重放回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantReceipt {
    pub request_id: String,
    pub coupon_id: u64,
    pub user_id: u64,
    pub coupon_grant_id: u64,
}

/// 同请求号且券、用户一致返回首次发放 ID；不一致返回冲突。
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

    fn rule(
        kind: DiscountType,
        min: i64,
        value: Option<i64>,
        percent: Option<f64>,
        free: u32,
    ) -> GrantRule {
        GrantRule {
            discount_type: kind,
            min_charge_cents: min,
            discount_value_cents: value,
            discount_percent: percent,
            free_minutes: free,
        }
    }

    #[test]
    fn amount_discount_basics() {
        assert_eq!(
            discount_for(&rule(DiscountType::Amount, 100, Some(50), None, 0), 99),
            Err(DiscountError::Threshold)
        );
        assert_eq!(
            discount_for(&rule(DiscountType::Amount, 100, Some(50), None, 0), 100).unwrap(),
            50
        );
        assert_eq!(
            discount_for(&rule(DiscountType::Amount, 0, Some(50), None, 0), 100).unwrap(),
            50
        );
        assert_eq!(
            discount_for(&rule(DiscountType::Amount, 0, Some(120), None, 0), 100).unwrap(),
            100,
            "减免封顶为原价"
        );
        assert!(discount_for(&rule(DiscountType::Amount, 0, Some(0), None, 0), 100).is_err());
        assert!(discount_for(&rule(DiscountType::Amount, 0, None, None, 0), 100).is_err());
    }

    #[test]
    fn percentage_discount_rounding() {
        // 50% → 应付 50 → 优惠 50。
        assert_eq!(
            discount_for(&rule(DiscountType::Percentage, 0, None, Some(50.0), 0), 100).unwrap(),
            50
        );
        // 50.5% → 应付 50.5 四舍五入 51 → 优惠 49（half away from zero）。
        assert_eq!(
            discount_for(&rule(DiscountType::Percentage, 0, None, Some(50.5), 0), 100).unwrap(),
            49
        );
        // 33.33% of 1000 → 应付 333.3 → 333 → 优惠 667。
        assert_eq!(
            discount_for(
                &rule(DiscountType::Percentage, 0, None, Some(33.33), 0),
                1000
            )
            .unwrap(),
            667
        );
        // 0.01% of 99 → 应付 0.0099 → 0 → 优惠 99。
        assert_eq!(
            discount_for(&rule(DiscountType::Percentage, 0, None, Some(0.01), 0), 99).unwrap(),
            99
        );
        assert!(discount_for(&rule(DiscountType::Percentage, 0, None, Some(0.0), 0), 100).is_err());
        assert!(
            discount_for(
                &rule(DiscountType::Percentage, 0, None, Some(100.0), 0),
                100
            )
            .is_err()
        );
        assert!(
            discount_for(
                &rule(DiscountType::Percentage, 0, None, Some(150.0), 0),
                100
            )
            .is_err()
        );
    }

    #[test]
    fn time_free_and_unknown() {
        // 时长券固定零费率：优惠恒 0（仍要过门槛）。
        assert_eq!(
            discount_for(&rule(DiscountType::TimeFree, 10, None, None, 30), 100).unwrap(),
            0
        );
        assert!(discount_for(&rule(DiscountType::TimeFree, 10, None, None, 0), 100).is_err());
        // 「未知类型返回 NotFound」由词表穷尽承接：match 无兜底分支，
        // 未知字面量在 DB 解码（parse）即被拒绝，编译器保证新增类型必处理。
        assert_eq!(
            discount_for(&rule(DiscountType::TimeFree, 10, None, None, 30), 9),
            Err(DiscountError::Threshold)
        );
        assert_eq!(
            discount_for(&rule(DiscountType::Amount, 0, Some(10), None, 0), 0),
            Err(DiscountError::Threshold)
        );
    }

    #[test]
    fn grant_rules() {
        let tpl = CouponTemplate {
            template_id: 1,
            status: CouponStatus::Active,
            total_quota: 2,
            per_user_quota: 1,
            valid_hours: 24,
            start_at: None,
            end_at: None,
        };
        let now = Utc::now();
        assert!(
            check_grantable(
                now,
                &tpl,
                CouponUsage {
                    total_granted: 1,
                    user_granted: 0
                }
            )
            .is_ok()
        );
        assert!(
            check_grantable(
                now,
                &tpl,
                CouponUsage {
                    total_granted: 2,
                    user_granted: 0
                }
            )
            .is_err()
        );
        assert!(
            check_grantable(
                now,
                &tpl,
                CouponUsage {
                    total_granted: 0,
                    user_granted: 1
                }
            )
            .is_err()
        );
        let disabled = CouponTemplate {
            status: CouponStatus::Disabled,
            ..tpl.clone()
        };
        assert!(
            check_grantable(
                now,
                &disabled,
                CouponUsage {
                    total_granted: 0,
                    user_granted: 0
                }
            )
            .is_err()
        );
        // 到期 = now + 24h。
        assert_eq!(grant_expiry(now, &tpl), now + chrono::Duration::hours(24));
        // 模板结束早于有效时长 → 以结束为上限。
        let bounded = CouponTemplate {
            end_at: Some(now + chrono::Duration::hours(2)),
            ..tpl.clone()
        };
        assert_eq!(
            grant_expiry(now, &bounded),
            now + chrono::Duration::hours(2)
        );
        // 重放：同请求同券同人复用；内容不同冲突。
        let receipt = GrantReceipt {
            request_id: "r1".into(),
            coupon_id: 1,
            user_id: 2,
            coupon_grant_id: 9,
        };
        assert_eq!(replay_grant(&receipt, "r1", 1, 2).unwrap(), 9);
        assert!(replay_grant(&receipt, "r2", 1, 2).is_err());
        assert!(replay_grant(&receipt, "r1", 3, 2).is_err());
    }
}

// ---------- 活动规则纯规则 ----------

/// 活动结束时间距今的上限。
pub const MAX_ACTIVITY_HORIZON: chrono::Duration = chrono::Duration::hours(365 * 24);

/// 活动名称字数上限。
pub const MAX_ACTIVITY_NAME_LENGTH: usize = 128;

/// 活动规则待校验参数：时间为 RFC3339 文本，由 normalize 统一解析。
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityRuleInput {
    pub name: String,
    /// 触发类型（TriggerType）。
    pub trigger_type: String,
    /// 活动发放的券模板。
    pub coupon_id: u64,
    /// 邀请人奖励券；邀请有奖必填。
    pub inviter_coupon_id: Option<u64>,
    /// 触发门槛，分。
    pub threshold_cents: i64,
    /// 同券模板和发放来源共享的份数上限，0 表示不限。
    pub max_grants: i32,
    /// 单人可领份数，0 沿用默认一份。
    pub per_user_limit: i32,
    /// coupon_activity_rule.status；留空时按 active 处理。
    pub status: String,
    /// RFC3339 开始时刻。
    pub start_at: String,
    /// RFC3339 结束时刻。
    pub end_at: String,
}

/// 校验活动并将时间统一为 UTC，空状态默认 active。
/// 满减门槛须为正，首充和节日门槛须为零；邀请奖励须配置邀请人券，金额门槛只要求非负。
/// 结束时间不得超过当前时间 365 天后。
/// 活动规则校验未通过（错误载荷无信息，调用方只区分成败；语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityRejected;

pub fn normalize_activity(
    rule: &ActivityRuleInput,
    now: chrono::DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>, ActivityStatus), ActivityRejected> {
    if rule.name.is_empty() || rule.name.chars().count() > MAX_ACTIVITY_NAME_LENGTH {
        return Err(ActivityRejected);
    }
    let trigger_type =
        ActivityTriggerType::parse(rule.trigger_type.as_str()).map_err(|_| ActivityRejected)?;
    if rule.coupon_id == 0 {
        return Err(ActivityRejected);
    }
    let start = chrono::DateTime::parse_from_rfc3339(&rule.start_at)
        .map_err(|_| ActivityRejected)?
        .with_timezone(&Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&rule.end_at)
        .map_err(|_| ActivityRejected)?
        .with_timezone(&Utc);
    if !(start < end) {
        return Err(ActivityRejected);
    }
    if end > now + MAX_ACTIVITY_HORIZON {
        return Err(ActivityRejected);
    }
    if rule.threshold_cents < 0 || rule.max_grants < 0 || rule.per_user_limit < 0 {
        return Err(ActivityRejected);
    }
    match trigger_type {
        ActivityTriggerType::ThresholdRedeem => {
            if rule.threshold_cents <= 0 {
                return Err(ActivityRejected);
            }
        }
        ActivityTriggerType::InviteReward => match rule.inviter_coupon_id {
            Some(id) if id != 0 => {}
            _ => return Err(ActivityRejected),
        },
        // 首充与节假日活动不设置门槛。
        ActivityTriggerType::FirstRecharge | ActivityTriggerType::Holiday => {
            if rule.threshold_cents != 0 {
                return Err(ActivityRejected);
            }
        }
    }
    let status = if rule.status.is_empty() {
        ActivityStatus::Active
    } else {
        ActivityStatus::parse(rule.status.as_str()).map_err(|_| ActivityRejected)?
    };
    Ok((start, end, status))
}

/// 活动发券有效期：取发券起算 valid_hours 小时、券模板结束、活动结束三者最早
/// 。
pub fn activity_grant_expiry(
    now: DateTime<Utc>,
    valid_hours: i32,
    coupon_end: Option<DateTime<Utc>>,
    rule_end: DateTime<Utc>,
) -> DateTime<Utc> {
    let mut expires = now + chrono::Duration::hours(valid_hours as i64);
    if let Some(end) = coupon_end
        && end < expires
    {
        expires = end;
    }
    if rule_end < expires {
        expires = rule_end;
    }
    expires
}

/// 单人份数归一：零或负数按一份处理，不表示不限量；总量不限由总份数字段控制
/// 。
pub fn per_user_allowance(per_user_limit: i32) -> i64 {
    if per_user_limit <= 0 {
        1
    } else {
        per_user_limit as i64
    }
}

/// 邀请资格：仅对邀请奖励检查非空邀请人和禁止自邀，其他触发不受限制
/// 。
pub fn invite_eligible(trigger_type: &str, inviter_id: u64, user_id: u64) -> bool {
    if ActivityTriggerType::parse(trigger_type) != Ok(ActivityTriggerType::InviteReward) {
        return true;
    }
    inviter_id != 0 && inviter_id != user_id
}

/// 由规则、用户、事件键和奖励方生成稳定的 32 字符发放号，不使用重试时间
/// 。
pub fn activity_event_id(rule_id: u64, user_id: u64, event_key: &str, scope: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut key = String::new();
    key.push_str(&rule_id.to_string());
    for s in scope {
        key.push('\0');
        key.push_str(s);
    }
    key.push('\0');
    key.push_str(&user_id.to_string());
    key.push('\0');
    key.push_str(event_key);
    let sum = Sha256::digest(key.as_bytes());
    sum[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// 触发类型到发放来源：邀请奖励归 invite_reward，其余活动归 activity
/// 。
pub fn grant_source_for(trigger_type: &str) -> &'static str {
    if ActivityTriggerType::parse(trigger_type) == Ok(ActivityTriggerType::InviteReward) {
        GrantSource::InviteReward.as_str()
    } else {
        GrantSource::Activity.as_str()
    }
}

/// 充值请求号生成稳定事件键。
pub fn activity_event_key_for_recharge(request_id: &str) -> String {
    format!("recharge:{request_id}")
}

/// 充电订单号生成稳定事件键。
pub fn activity_event_key_for_order(order_no: &str) -> String {
    format!("order:{order_no}")
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    fn base_rule() -> ActivityRuleInput {
        ActivityRuleInput {
            name: "满 50 减 5".into(),
            trigger_type: "threshold_redeem".into(),
            coupon_id: 1,
            inviter_coupon_id: None,
            threshold_cents: 5000,
            max_grants: 0,
            per_user_limit: 0,
            status: "".into(),
            start_at: "2026-01-01T00:00:00Z".into(),
            end_at: "2026-12-31T00:00:00Z".into(),
        }
    }

    #[test]
    fn normalize_activity_rules() {
        let now = Utc::now();
        // 合法：默认 status=active。
        let (s, e, st) = normalize_activity(&base_rule(), now).unwrap();
        assert_eq!(st, ActivityStatus::Active);
        assert!(s < e);
        // 满减门槛必须为正。
        let mut r = base_rule();
        r.threshold_cents = 0;
        assert!(normalize_activity(&r, now).is_err());
        // 首充/节日门槛必须为零。
        let mut r = base_rule();
        r.trigger_type = "first_recharge".into();
        assert!(normalize_activity(&r, now).is_err());
        r.threshold_cents = 0;
        assert!(normalize_activity(&r, now).is_ok());
        // 邀请有奖必须配置邀请人券。
        let mut r = base_rule();
        r.trigger_type = "invite_reward".into();
        r.threshold_cents = 0;
        assert!(normalize_activity(&r, now).is_err());
        r.inviter_coupon_id = Some(2);
        assert!(normalize_activity(&r, now).is_ok());
        // 时间窗口与上限。
        let mut r = base_rule();
        r.start_at = "2027-01-02T00:00:00Z".into();
        assert!(normalize_activity(&r, now).is_err());
        let mut r = base_rule();
        r.end_at = (now + chrono::Duration::hours(24 * 366)).to_rfc3339();
        assert!(normalize_activity(&r, now).is_err());
        // 非法触发类型与空状态值。
        let mut r = base_rule();
        r.trigger_type = "cashback".into();
        assert!(normalize_activity(&r, now).is_err());
        let mut r = base_rule();
        r.status = "paused".into();
        assert!(normalize_activity(&r, now).is_err());
    }

    #[test]
    fn expiry_allowance_and_eligibility() {
        let now = Utc::now();
        let rule_end = now + chrono::Duration::hours(10);
        // 有效期取三者最早。
        assert_eq!(activity_grant_expiry(now, 24, None, rule_end), rule_end);
        let coupon_end = now + chrono::Duration::hours(5);
        assert_eq!(
            activity_grant_expiry(now, 24, Some(coupon_end), rule_end),
            coupon_end
        );
        assert_eq!(
            activity_grant_expiry(now, 2, Some(coupon_end), rule_end),
            now + chrono::Duration::hours(2)
        );
        // 单人份数归一。
        assert_eq!(per_user_allowance(0), 1);
        assert_eq!(per_user_allowance(-3), 1);
        assert_eq!(per_user_allowance(3), 3);
        // 邀请资格：仅 invite_reward 检查，禁止自邀。
        assert!(invite_eligible("first_recharge", 0, 1));
        assert!(!invite_eligible("invite_reward", 0, 1));
        assert!(!invite_eligible("invite_reward", 2, 2));
        assert!(invite_eligible("invite_reward", 2, 1));
    }

    #[test]
    fn event_id_is_stable_and_scoped() {
        // 同输入同输出；scope 与用户、事件键参与哈希。
        let a = activity_event_id(7, 9, "order:C1", &[]);
        let b = activity_event_id(7, 9, "order:C1", &[]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert_ne!(a, activity_event_id(8, 9, "order:C1", &[]));
        assert_ne!(a, activity_event_id(7, 10, "order:C1", &[]));
        assert_ne!(a, activity_event_id(7, 9, "order:C2", &[]));
        // 邀请人 scope 与受益人发放号不同。
        assert_ne!(activity_event_id(7, 9, "order:C1", &["inviter"]), a);
        // 来源映射：invite_reward 触发 → invite_reward 来源，其余 → activity。
        assert_eq!(grant_source_for("invite_reward"), "invite_reward");
        assert_eq!(grant_source_for("threshold_redeem"), "activity");
        // 事件键前缀。
        assert_eq!(activity_event_key_for_recharge("R1"), "recharge:R1");
        assert_eq!(activity_event_key_for_order("C1"), "order:C1");
    }
}

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let coupon = [
            (CouponStatus::Active, "active"),
            (CouponStatus::Disabled, "disabled"),
        ];
        for (v, s) in coupon {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let activity = [
            (ActivityStatus::Active, "active"),
            (ActivityStatus::Disabled, "disabled"),
        ];
        for (v, s) in activity {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let dtype = [
            (DiscountType::Amount, "amount"),
            (DiscountType::Percentage, "percentage"),
            (DiscountType::TimeFree, "time_free"),
        ];
        for (v, s) in dtype {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let trigger = [
            (ActivityTriggerType::FirstRecharge, "first_recharge"),
            (ActivityTriggerType::InviteReward, "invite_reward"),
            (ActivityTriggerType::ThresholdRedeem, "threshold_redeem"),
            (ActivityTriggerType::Holiday, "holiday"),
        ];
        for (v, s) in trigger {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let grant = [
            (GrantStatus::Unused, "unused"),
            (GrantStatus::Used, "used"),
            (GrantStatus::Expired, "expired"),
        ];
        for (v, s) in grant {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let source = [
            (GrantSource::Register, "register"),
            (GrantSource::Activity, "activity"),
            (GrantSource::Invite, "invite"),
            (GrantSource::Manual, "manual"),
            (GrantSource::InviteReward, "invite_reward"),
        ];
        for (v, s) in source {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
