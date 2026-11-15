//! 优惠券模板与人工发放用例：权限核对、校验与发放编排（锁序：请求回执（读）
//! → 用户行 → 券模板行 → 发放行计数；模板锁之后不得反向等待用户或钱包）。
//! 审计 module/target 均为 "coupon"；coupon 主键沿用 AUTO_INCREMENT。
//! SQL 原语在 repo/coupon.rs。

use crate::error::{CouponError, CouponRedeemError};
use crate::repo::coupon;
use chargepilot_domain::marketing::GrantStatus;
use chargepilot_mod_identity::repo as identity_repo;
use chargepilot_mod_ops::{Profile, repo as ops_repo};
use chrono::Utc;
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

pub use crate::repo::coupon::{CouponRow, GrantWithRule};

/// 模板更新输入：只允许修改名称和启停状态，不改变优惠和发放上限。
#[derive(Debug, Clone)]
pub struct CouponUpdate {
    pub name: String,
    pub status: String,
}

/// 人工发放输入。
#[derive(Debug, Clone)]
pub struct GrantInput {
    /// UUID 请求号，重试沿用同一号。
    pub request_id: String,
    /// 目标用户主键。
    pub user_id: u64,
}

/// 发放结果：发放主键和请求号，重放不生成新编号。
#[derive(Debug, Clone, Serialize)]
pub struct GrantResult {
    pub coupon_grant_id: u64,
    pub request_id: String,
}

/// 发放统计：未用但已到期计为 expired。
#[derive(Debug, Clone, Serialize)]
pub struct CouponStats {
    pub total_quota: u32,
    pub granted_count: i64,
    pub unused_count: i64,
    pub used_count: i64,
    pub expired_count: i64,
    pub usage_rate: f64,
}

/// 实时权限判定。
fn has_permission(profile: &Profile, code: &str) -> bool {
    profile.permissions.iter().any(|p| p == code)
}

/// 自由文本校验：去首尾空白后非空且按 rune 计数不超限。
fn valid_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max
}

/// UUID 请求号校验（8-4-4-4-12 十六进制）。
fn valid_request_id(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, ch) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *ch != b'-' {
                    return false;
                }
            }
            _ => {
                if !ch.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// 优惠参数三选一互斥校验：
/// amount 须为正分值；percentage 严格在 0 与 100 之间且最多两位小数；
/// time_free 为 1–1440 分钟。其余列必须为空。
fn valid_discount(
    discount_type: &str,
    discount_value_cents: Option<i64>,
    discount_percent: Option<f64>,
    free_minutes: Option<u32>,
) -> bool {
    let parsed = match chargepilot_domain::marketing::DiscountType::parse(discount_type) {
        Ok(v) => v,
        Err(_) => return false,
    };
    match parsed {
        chargepilot_domain::marketing::DiscountType::Amount => {
            matches!(discount_value_cents, Some(v) if v > 0)
                && discount_percent.is_none()
                && free_minutes.is_none()
        }
        chargepilot_domain::marketing::DiscountType::Percentage => {
            let Some(p) = discount_percent else {
                return false;
            };
            // 严格 (0,100) 开区间，且两位小数内可精确表示。
            if !(p > 0.0 && p < 100.0) {
                return false;
            }
            use rust_decimal::prelude::FromPrimitive;
            let Some(d) = rust_decimal::Decimal::from_f64(p) else {
                return false;
            };
            let rounded =
                d.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
            rounded == d && discount_value_cents.is_none() && free_minutes.is_none()
        }
        chargepilot_domain::marketing::DiscountType::TimeFree => {
            matches!(free_minutes, Some(m) if m > 0 && m <= 1440)
                && discount_percent.is_none()
                && discount_value_cents.is_none()
        }
    }
}

/// 后台模板列表：核对 coupon.read 后倒序读取未删除模板。
pub async fn list_coupons(
    pool: &MySqlPool,
    actor: &Profile,
) -> Result<Vec<CouponRow>, CouponError> {
    if !has_permission(actor, chargepilot_mod_ops::perms::COUPON_READ) {
        return Err(CouponError::Forbidden);
    }
    coupon::coupon_rows_desc(pool).await
}

/// 创建启用模板并同事务保存审计：客户端主键必须为 0，
/// 状态强制 active（忽略提交值），优惠参数三选一互斥。
pub async fn create_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    in_row: CouponRow,
) -> Result<CouponRow, CouponError> {
    if !has_permission(actor, chargepilot_mod_ops::perms::COUPON_CREATE) {
        return Err(CouponError::Forbidden);
    }
    if in_row.id != 0
        || !valid_text(&in_row.name, 128)
        || in_row.min_charge_cents < 0
        || in_row.valid_hours == 0
        || in_row.valid_hours > 87_600
        || in_row.per_user_quota == 0
        || matches!((&in_row.end_at, &in_row.start_at), (Some(end), Some(start)) if end <= start)
    {
        return Err(CouponError::Input("优惠券名称、有效期或额度无效".into()));
    }
    if !valid_discount(
        &in_row.discount_type,
        in_row.discount_value_cents,
        in_row.discount_percent,
        in_row.free_minutes,
    ) {
        return Err(CouponError::Input(
            "优惠参数无效：满减分值须大于 0，折扣为 0–100 之间，免费时长为 1–1440 分钟".into(),
        ));
    }
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let id = coupon::insert_coupon_tx(
        c,
        &in_row.name,
        &in_row.discount_type,
        in_row.discount_value_cents,
        in_row.discount_percent,
        in_row.free_minutes,
        in_row.min_charge_cents,
        in_row.valid_hours,
        in_row.total_quota,
        in_row.per_user_quota,
        in_row.start_at,
        in_row.end_at,
    )
    .await?;
    let mut row = in_row;
    row.id = id;
    row.status = chargepilot_domain::marketing::CouponStatus::Active
        .as_str()
        .to_owned();
    let after = serde_json::to_value(&row).unwrap_or(serde_json::Value::Null);
    ops_repo::append_audit_tx(
        &mut *c,
        actor,
        "coupon",
        "create",
        "coupon",
        &id.to_string(),
        &serde_json::Value::Null,
        &after,
        client_ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// 更新模板名称与启停状态并同事务保存审计：
/// 锁定读取旧值，优惠与额度不可修改。
pub async fn update_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    id: u64,
    update: CouponUpdate,
) -> Result<CouponUpdate, CouponError> {
    if !has_permission(actor, chargepilot_mod_ops::perms::COUPON_UPDATE) {
        return Err(CouponError::Forbidden);
    }
    if id == 0
        || !valid_text(&update.name, 128)
        || chargepilot_domain::marketing::CouponStatus::parse(update.status.as_str()).is_err()
    {
        return Err(CouponError::Input("名称或状态无效".into()));
    }
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(before) = coupon::load_coupon(&mut *c, id, true).await? else {
        return Err(CouponError::NotFound);
    };
    coupon::update_name_status_tx(&mut *c, id, &update.name, &update.status).await?;
    ops_repo::append_audit_tx(
        &mut *c,
        actor,
        "coupon",
        "update",
        "coupon",
        &id.to_string(),
        &serde_json::to_value(&before).unwrap_or(serde_json::Value::Null),
        &serde_json::json!({"name": update.name, "status": update.status}),
        client_ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(update)
}

/// 发放统计：unused 但已到期计为 expired。
pub async fn coupon_stats(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
) -> Result<CouponStats, CouponError> {
    if !has_permission(actor, chargepilot_mod_ops::perms::COUPON_READ) {
        return Err(CouponError::Forbidden);
    }
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(coupon) = coupon::load_coupon(&mut *c, id, false).await? else {
        return Err(CouponError::NotFound);
    };
    let (granted, unused, used, expired) = coupon::grant_stats_counts(&mut *c, id).await?;
    tx.commit().await?;
    Ok(CouponStats {
        total_quota: coupon.total_quota,
        granted_count: granted,
        unused_count: unused,
        used_count: used,
        expired_count: expired,
        usage_rate: if granted > 0 {
            used as f64 / granted as f64
        } else {
            0.0
        },
    })
}

/// 人工发放：核对 coupon.grant 权限后向指定用户发券，
/// 发放与审计在同一事务提交；重试沿用请求号。发放条件不满足按冲突处理。
pub async fn grant_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    coupon_id: u64,
    input: GrantInput,
) -> Result<GrantResult, CouponError> {
    if !has_permission(actor, chargepilot_mod_ops::perms::COUPON_GRANT) {
        return Err(CouponError::Forbidden);
    }
    if coupon_id == 0 || input.user_id == 0 || !valid_request_id(&input.request_id) {
        return Err(CouponError::Input("请求编号或用户编号无效".into()));
    }
    let grant_id = grant_in_tx_with_audit(
        pool,
        coupon_id,
        input.user_id,
        &input.request_id,
        Some(actor),
        client_ip,
        request_id,
    )
    .await?;
    Ok(GrantResult {
        coupon_grant_id: grant_id,
        request_id: input.request_id,
    })
}

/// 发放核心：先检查首次请求重放，再按 用户 → 模板 → 发放行
/// 加锁，复查请求和额度后发券。同请求且用户、模板一致时返回首次发放 ID；
/// 条件不满足或内容不一致返回冲突。audit 上下文给定时与发放同事务写审计
/// （先发券后追加审计）；活动发放复用时不传。
pub async fn grant_in_tx_with_audit(
    pool: &MySqlPool,
    coupon_id: u64,
    user_id: u64,
    request_id: &str,
    audit_actor: Option<&Profile>,
    audit_ip: &str,
    audit_request_id: &str,
) -> Result<u64, CouponError> {
    if coupon_id == 0 || user_id == 0 || !valid_request_id(request_id) {
        return Err(CouponError::Conflict("优惠券发放条件不满足".into()));
    }
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 首次重放检查（无锁读；命中即同请求复用，读事务无写入直接提交）。
    if let Some(receipt) = coupon::find_grant_receipt(&mut *c, request_id, false).await? {
        let grant_id = replay_grant_or_conflict(receipt, request_id, coupon_id, user_id)?;
        tx.commit().await?;
        return Ok(grant_id);
    }
    // 用户行锁（锁序第一环）。
    let Some(status) = identity_repo::lock_status_tx(&mut *c, user_id).await? else {
        return Err(CouponError::Conflict("优惠券发放条件不满足".into()));
    };
    // 模板锁 + 发放计数（模板锁之后不能反向等待用户或钱包）。
    let Some(row) = coupon::load_coupon(&mut *c, coupon_id, true).await? else {
        return Err(CouponError::NotFound);
    };
    let (total_granted, user_granted) =
        coupon::grant_counts_tx(&mut *c, user_id, coupon_id).await?;
    // 锁后二次重放检查（FOR SHARE 防并发写入）。
    if let Some(receipt) = coupon::find_grant_receipt(&mut *c, request_id, true).await? {
        let grant_id = replay_grant_or_conflict(receipt, request_id, coupon_id, user_id)?;
        tx.commit().await?;
        return Ok(grant_id);
    }
    // 准入：用户 active + 模板启用/时间有效/额度未满。
    let template = chargepilot_domain::marketing::CouponTemplate {
        template_id: row.id,
        // 行读取经 DB 词表严格解码，视图 String 此处回 parse（非法即输入错误）。
        status: chargepilot_domain::marketing::CouponStatus::parse(&row.status)
            .map_err(|_| CouponError::Input("优惠券状态无效".into()))?,
        total_quota: row.total_quota,
        per_user_quota: row.per_user_quota,
        valid_hours: row.valid_hours,
        start_at: row.start_at,
        end_at: row.end_at,
    };
    let usage = chargepilot_domain::marketing::CouponUsage {
        total_granted,
        user_granted,
    };
    let now = Utc::now();
    if status != "active"
        || chargepilot_domain::marketing::check_grantable(now, &template, usage).is_err()
    {
        return Err(CouponError::Conflict("优惠券发放条件不满足".into()));
    }
    let expires_at = chargepilot_domain::marketing::grant_expiry(now, &template);
    // 发放记录与请求回执（唯一键兜底并发重放）。
    let grant_id = coupon::insert_grant_tx(c, coupon_id, user_id, expires_at, request_id).await?;
    coupon::insert_grant_request_tx(c, request_id, coupon_id, user_id, grant_id).await?;
    // 审计与发放同事务（顺序：先发券后追加审计）。
    if let Some(actor) = audit_actor {
        let after = serde_json::json!({
            "request_id": request_id,
            "user_id": user_id.to_string(),
        });
        ops_repo::append_audit_tx(
            &mut *c,
            actor,
            "coupon",
            "grant",
            "coupon",
            &coupon_id.to_string(),
            &serde_json::Value::Null,
            &after,
            audit_ip,
            audit_request_id,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(grant_id)
}

/// 重放判定：同请求同券同人返回首次发放 ID；不一致返回冲突。
fn replay_grant_or_conflict(
    receipt: chargepilot_domain::marketing::GrantReceipt,
    request_id: &str,
    coupon_id: u64,
    user_id: u64,
) -> Result<u64, CouponError> {
    match chargepilot_domain::marketing::replay_grant(&receipt, request_id, coupon_id, user_id) {
        Ok(grant_id) => Ok(grant_id),
        Err(_) => Err(CouponError::Conflict("发放请求与首次请求不一致".into())),
    }
}

// ---------- 报价与核销 ----------

/// 发放行的优惠计算输入（domain GrantRule）。
fn grant_rule(
    g: &GrantWithRule,
) -> Result<chargepilot_domain::marketing::GrantRule, CouponRedeemError> {
    Ok(chargepilot_domain::marketing::GrantRule {
        discount_type: chargepilot_domain::marketing::DiscountType::parse(&g.discount_type)
            .map_err(|_| CouponRedeemError::NotFound)?,
        min_charge_cents: g.min_charge_cents,
        discount_value_cents: g.discount_value_cents,
        discount_percent: g.discount_percent,
        free_minutes: g.free_minutes,
    })
}

/// 计算本人未用、未到期券对原价的优惠，单位分，不核销券。
pub async fn quote(
    pool: &MySqlPool,
    user_id: u64,
    grant_id: u64,
    total_cents: i64,
) -> Result<i64, CouponRedeemError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(grant) = coupon::load_grant(&mut *c, user_id, grant_id, false).await? else {
        return Err(CouponRedeemError::NotFound);
    };
    if grant.status != GrantStatus::Unused || grant.expires_at <= Utc::now() {
        return Err(CouponRedeemError::Exhausted);
    }
    tx.commit().await?;
    chargepilot_domain::marketing::discount_for(&grant_rule(&grant)?, total_cents).map_err(|e| {
        match e {
            chargepilot_domain::marketing::DiscountError::Threshold => CouponRedeemError::Threshold,
            chargepilot_domain::marketing::DiscountError::NotFound => CouponRedeemError::NotFound,
        }
    })
}

/// 在资金事务内锁定并核销券：同一券和支付记录重放沿用
/// 首次结果；优惠须等于支付时保存的冻结额，不能按实付额再算；核销随调用方
/// 资金事务一起提交或回滚。reference 为核销单号（订单号或 PENDING- 派生）。
pub async fn redeem_in_tx(
    c: &mut sqlx::MySqlConnection,
    user_id: u64,
    grant_id: u64,
    payment_order_id: u64,
    total_cents: i64,
    discount_cents: i64,
    reference: &str,
) -> Result<(), CouponRedeemError> {
    if user_id == 0
        || grant_id == 0
        || payment_order_id == 0
        || reference.is_empty()
        || discount_cents < 0
        || discount_cents > total_cents
    {
        return Err(CouponRedeemError::Mismatch);
    }
    let Some(grant) = coupon::load_grant(c, user_id, grant_id, true).await? else {
        return Err(CouponRedeemError::NotFound);
    };
    // 已用：仅同一支付的同额核销视为重放成功。
    if grant.status == GrantStatus::Used {
        if grant.used_payment_order_id != Some(payment_order_id) {
            return Err(CouponRedeemError::Exhausted);
        }
        let Some(receipt) = coupon::find_redemption(c, payment_order_id).await? else {
            return Err(CouponRedeemError::Mismatch);
        };
        if receipt.coupon_id != grant.coupon_id
            || receipt.discount_cents != discount_cents
            || receipt.reference != reference
        {
            return Err(CouponRedeemError::Mismatch);
        }
        return Ok(());
    }
    if grant.status != GrantStatus::Unused || grant.expires_at <= Utc::now() {
        return Err(CouponRedeemError::Exhausted);
    }
    // 冻结优惠必须与当前规则计算一致。
    let discount = chargepilot_domain::marketing::discount_for(&grant_rule(&grant)?, total_cents)
        .map_err(|e| match e {
        chargepilot_domain::marketing::DiscountError::Threshold => CouponRedeemError::Threshold,
        chargepilot_domain::marketing::DiscountError::NotFound => CouponRedeemError::NotFound,
    })?;
    if discount != discount_cents {
        return Err(CouponRedeemError::Mismatch);
    }
    // 条件更新到 used：并发竞争失败按已用处理。
    let updated = coupon::mark_grant_used_tx(c, grant_id, user_id, payment_order_id).await?;
    if updated != 1 {
        return Err(CouponRedeemError::Exhausted);
    }
    coupon::insert_redemption_tx(
        c,
        grant.coupon_id,
        user_id,
        payment_order_id,
        discount_cents,
        reference,
    )
    .await?;
    Ok(())
}

/// 充电行尚不存在时的稳定核销单号。
pub fn pending_charge_no_for(payment_order_id: u64) -> String {
    format!("PENDING-{payment_order_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_discount_rules() {
        // amount：正分值，其余列为空。
        assert!(valid_discount("amount", Some(500), None, None));
        assert!(!valid_discount("amount", Some(0), None, None));
        assert!(!valid_discount("amount", Some(500), Some(10.0), None));
        // percentage：(0,100) 开区间且两位小数。
        assert!(valid_discount("percentage", None, Some(80.0), None));
        assert!(valid_discount("percentage", None, Some(33.33), None));
        assert!(!valid_discount("percentage", None, Some(0.0), None));
        assert!(!valid_discount("percentage", None, Some(100.0), None));
        assert!(!valid_discount("percentage", None, Some(33.333), None));
        assert!(!valid_discount("percentage", None, None, None));
        // time_free：1–1440 分钟。
        assert!(valid_discount("time_free", None, None, Some(30)));
        assert!(!valid_discount("time_free", None, None, Some(0)));
        assert!(!valid_discount("time_free", None, None, Some(1441)));
        // 未知类型一律拒绝。
        assert!(!valid_discount("cashback", Some(500), None, None));
    }

    #[test]
    fn request_id_and_text() {
        assert!(valid_request_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(valid_request_id("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!valid_request_id("550e8400e29b41d4a716446655440000"));
        assert!(!valid_request_id("550e8400-e29b-41d4-a716-44665544000"));
        assert!(!valid_request_id(""));
        assert!(valid_text(" 新客立减券 ", 128));
        assert!(!valid_text("  ", 128));
        assert!(!valid_text(&"长".repeat(129), 128));
        assert!(valid_text(&"长".repeat(128), 128));
    }
}
