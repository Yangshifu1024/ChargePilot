//! 活动规则管理与事件触发的幂等发放用例：发放锁序：规则主键 → 券模板主键
//! （升序）→ 发放行计数；模板锁之后不得反向等待用户或钱包。发放号 =
//! sha256(规则+用户+事件键) 前 16 字节 hex，重试不产生新券；唯一键冲突按
//! "已发放"处理。权限码：coupon.activity.read / coupon.activity.manage，
//! 审计 module/target = "coupon_activity"。SQL 在 repo/activity.rs。

use crate::error::CouponError;
use crate::repo::activity;
use chargepilot_domain::marketing::{
    ActivityTriggerType, CouponStatus, activity_event_id, activity_grant_expiry, grant_source_for,
    invite_eligible, per_user_allowance,
};
use chargepilot_mod_ops::{Profile, repo as ops_repo};
use chrono::{DateTime, Utc};
use sqlx::{Acquire, MySqlConnection, MySqlPool};

pub use crate::repo::activity::{ActivityEvent, ActivityRow};

/// 规则的发放结果，重放不会新增券。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivityResult {
    pub rule_id: u64,
    pub coupon_id: u64,
    /// 本次已给邀请人发券，或此前已发过同一事件奖励。
    pub inviter_rewarded: bool,
    /// 受益人发放键已存在（重放）。
    pub already_granted: bool,
}

use crate::repo::activity::{ActivityCouponRow, ActivityRuleRow};

/// 模板状态与有效时间检查；缺失模板不可用。
fn usable_coupon(coupon: &ActivityCouponRow, now: DateTime<Utc>) -> bool {
    coupon.status == CouponStatus::Active
        && coupon.start_at.is_none_or(|s| now >= s)
        && coupon.end_at.is_none_or(|e| now < e)
}

/// 发放有效期：有效小时、模板结束、活动结束三者最早。
fn activity_expiry(
    rule: &ActivityRuleRow,
    coupon: &ActivityCouponRow,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    activity_grant_expiry(now, coupon.valid_hours as i32, coupon.end_at, rule.end_at)
}

/// 事件触发的幂等发放：候选 → 锁读规则 → 模板锁 → 逐规则发放。
/// 使用调用方事务（充值回调/订单完成的资金事务内调用），失败整体回滚；
/// 条件不满足的规则逐条跳过，不中断其他规则。
pub async fn apply_in_tx(
    c: &mut MySqlConnection,
    event: &ActivityEvent,
) -> Result<Vec<ActivityResult>, CouponError> {
    if event.user_id == 0 || event.event_key.is_empty() {
        return Ok(vec![]);
    }
    let rule_ids = activity::candidate_rule_ids(c, event).await?;
    let rules = activity::lock_rules(c, event, &rule_ids).await?;
    let mut coupon_ids = Vec::with_capacity(rules.len() * 2);
    for rule in &rules {
        coupon_ids.push(rule.coupon_id);
        if let Some(id) = rule.inviter_coupon_id.filter(|v| *v > 0) {
            coupon_ids.push(id);
        }
    }
    let coupons = activity::lock_coupons(c, &coupon_ids).await?;
    let mut out = Vec::with_capacity(rules.len());
    for rule in &rules {
        match grant_from_rule(c, rule, event, &coupons).await {
            Ok(Some(result)) => out.push(result),
            Ok(None) => {}
            Err(CouponError::Storage(e)) => return Err(CouponError::Storage(e)),
            Err(_) => {}
        }
    }
    Ok(out)
}

/// 独立事务版发放（测试与无外部事务场景）：自开短事务调用 apply_in_tx。
pub async fn apply(
    pool: &MySqlPool,
    event: &ActivityEvent,
) -> Result<Vec<ActivityResult>, CouponError> {
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let results = apply_in_tx(&mut *c, event).await?;
    tx.commit().await?;
    Ok(results)
}

/// 单规则发放判定与写入。返回 None 表示规则不适用。
/// 邀请人未建立（无完成充电/到账充值）时整条规则不发；邀请人券不可用或
/// 份数用尽时只跳过邀请人奖励。
async fn grant_from_rule(
    c: &mut MySqlConnection,
    rule: &ActivityRuleRow,
    event: &ActivityEvent,
    coupons: &std::collections::HashMap<u64, ActivityCouponRow>,
) -> Result<Option<ActivityResult>, CouponError> {
    let mut out = ActivityResult {
        rule_id: rule.id,
        coupon_id: rule.coupon_id,
        inviter_rewarded: false,
        already_granted: false,
    };
    let event_id = activity_event_id(rule.id, event.user_id, &event.event_key, &[]);
    // 重放：同发放号且同用户按已发放处理；不同用户视为冲突。
    if let Some(receipt) = activity::find_receipt(c, &event_id).await? {
        if receipt.user_id != event.user_id {
            return Err(CouponError::Conflict("发放请求与首次请求不一致".into()));
        }
        out.coupon_id = receipt.coupon_id;
        out.already_granted = true;
        if ActivityTriggerType::parse(rule.trigger_type.as_str())
            == Ok(ActivityTriggerType::InviteReward)
            && event.inviter_id != 0
        {
            let inviter_event_id =
                activity_event_id(rule.id, event.inviter_id, &event.event_key, &["inviter"]);
            out.inviter_rewarded =
                activity::activity_grant_exists(c, &inviter_event_id, event.inviter_id).await?;
        }
        return Ok(Some(out));
    }
    let Some(coupon) = coupons.get(&rule.coupon_id) else {
        return Ok(None);
    };
    if !usable_coupon(coupon, event.now) {
        return Ok(None);
    }
    if ActivityTriggerType::parse(rule.trigger_type.as_str())
        == Ok(ActivityTriggerType::InviteReward)
    {
        if !invite_eligible(&rule.trigger_type, event.inviter_id, event.user_id) {
            return Ok(None);
        }
        if !activity::is_established_user(c, event.inviter_id).await? {
            return Ok(None);
        }
    }
    if rule.max_grants > 0 {
        let n = activity::count_rule_grants(c, rule).await?;
        if n >= rule.max_grants as i64 {
            return Ok(None);
        }
    }
    let allowance = per_user_allowance(rule.per_user_limit);
    let n = activity::count_user_grants(c, rule.coupon_id, event.user_id).await?;
    if n >= allowance {
        return Ok(None);
    }
    let source = grant_source_for(&rule.trigger_type);
    let expires_at = activity_expiry(rule, coupon, event.now);
    out.already_granted = activity::save_grant(
        c,
        rule.coupon_id,
        event.user_id,
        source,
        expires_at,
        &event_id,
        true,
    )
    .await?;
    if out.already_granted {
        return Ok(Some(out));
    }
    // 邀请人奖励：券不可用或份数用尽只跳过邀请人部分，受益人券保持发放。
    if ActivityTriggerType::parse(rule.trigger_type.as_str())
        == Ok(ActivityTriggerType::InviteReward)
        && let Some(inviter_coupon_id) = rule.inviter_coupon_id.filter(|v| *v > 0)
    {
        let Some(inviter_coupon) = coupons.get(&inviter_coupon_id) else {
            return Ok(Some(out));
        };
        if !usable_coupon(inviter_coupon, event.now) {
            return Ok(Some(out));
        }
        let n = activity::count_user_grants(c, inviter_coupon_id, event.inviter_id).await?;
        if n >= allowance {
            return Ok(Some(out));
        }
        let inviter_event_id =
            activity_event_id(rule.id, event.inviter_id, &event.event_key, &["inviter"]);
        let expires_at = activity_expiry(rule, inviter_coupon, event.now);
        let _ = activity::save_grant(
            c,
            inviter_coupon_id,
            event.inviter_id,
            chargepilot_domain::marketing::GrantSource::InviteReward.as_str(),
            expires_at,
            &inviter_event_id,
            false,
        )
        .await?;
        out.inviter_rewarded = true;
    }
    Ok(Some(out))
}

// ---------- 后台活动规则管理 ----------

/// 按状态和名称分页查询规则，补充各券模板实际发放量。
/// 状态筛选项已按两值白名单校验后原样作为 WHERE 参数。
pub async fn list_activity_rules(
    pool: &MySqlPool,
    actor: &Profile,
    status: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<ActivityRow>, i64), CouponError> {
    if !has_activity_read(actor) {
        return Err(CouponError::Forbidden);
    }
    if chargepilot_domain::page::PageRequest::user(page, page_size).is_err() {
        return Err(CouponError::Input("分页或活动状态无效".into()));
    }
    if !status.is_empty() && chargepilot_domain::marketing::ActivityStatus::parse(status).is_err() {
        return Err(CouponError::Input("分页或活动状态无效".into()));
    }
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let (rows, total) = activity::rule_page(&mut *c, status, keyword, page, page_size).await?;
    // 批量统计活动/邀请来源发放量（仅这两种来源计入活动口径）。
    let coupon_ids: Vec<u64> = rows.iter().map(|r| r.3).collect();
    let counts = activity::grant_counts_by_coupon(&mut *c, &coupon_ids).await?;
    tx.commit().await?;
    Ok((
        rows.into_iter()
            .map(
                |(
                    id,
                    name,
                    trigger_type,
                    coupon_id,
                    coupon_name,
                    inviter_coupon_id,
                    threshold_cents,
                    max_grants,
                    per_user_limit,
                    status,
                    start_at,
                    end_at,
                    created_at,
                )| ActivityRow {
                    granted_count: counts.get(&coupon_id).copied().unwrap_or(0),
                    id,
                    name,
                    trigger_type,
                    coupon_id,
                    coupon_name,
                    inviter_coupon_id,
                    threshold_cents,
                    max_grants,
                    per_user_limit,
                    status,
                    start_at,
                    end_at,
                    created_at,
                },
            )
            .collect(),
        total,
    ))
}

/// 创建活动规则并同事务审计：奖励券须存在且启用（FOR SHARE），
/// 审计快照为归一化后的输入。
pub async fn create_activity_rule(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    input: chargepilot_domain::marketing::ActivityRuleInput,
) -> Result<u64, CouponError> {
    if !actor
        .permissions
        .iter()
        .any(|p| p == "coupon.activity.manage")
    {
        return Err(CouponError::Forbidden);
    }
    let (start_at, end_at, status) = normalize_or_input(&input, Utc::now())?;
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    require_activity_coupons(&mut *c, input.coupon_id, input.inviter_coupon_id).await?;
    let id = activity::insert_rule_tx(
        c,
        &input.name,
        &input.trigger_type,
        input.coupon_id,
        input.inviter_coupon_id,
        input.threshold_cents,
        input.max_grants,
        input.per_user_limit,
        status.as_str(),
        start_at,
        end_at,
    )
    .await?;
    let after = audit_after(&input, status.as_str(), start_at, end_at);
    ops_repo::append_audit_tx(
        &mut *c,
        actor,
        "coupon_activity",
        "create",
        "coupon_activity",
        &id.to_string(),
        &serde_json::Value::Null,
        &after,
        client_ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// 更新活动规则：触发类型保持不变；先锁规则再校验奖励券，
/// 更新和审计同事务。
pub async fn update_activity_rule(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    id: u64,
    input: chargepilot_domain::marketing::ActivityRuleInput,
) -> Result<(), CouponError> {
    if !actor
        .permissions
        .iter()
        .any(|p| p == "coupon.activity.manage")
    {
        return Err(CouponError::Forbidden);
    }
    if id == 0 {
        return Err(CouponError::Input("活动规则无效".into()));
    }
    let (start_at, end_at, status) = normalize_or_input(&input, Utc::now())?;
    let mut conn = pool.acquire().await?;
    crate::repo::coupon::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 锁定读旧规则（触发类型不可变；同时作为存在性判定）。
    let Some((before_name, before_trigger)) = activity::lock_rule_tx(&mut *c, id).await? else {
        return Err(CouponError::NotFound);
    };
    if before_trigger != input.trigger_type {
        return Err(CouponError::Input("活动触发类型不可修改".into()));
    }
    require_activity_coupons(&mut *c, input.coupon_id, input.inviter_coupon_id).await?;
    let changed = activity::update_rule_tx(
        c,
        id,
        &input.name,
        input.coupon_id,
        input.inviter_coupon_id,
        input.threshold_cents,
        input.max_grants,
        input.per_user_limit,
        status.as_str(),
        start_at,
        end_at,
    )
    .await?;
    if !changed {
        return Err(CouponError::NotFound);
    }
    let before_json = serde_json::json!({"name": before_name, "trigger_type": before_trigger});
    let after = audit_after(&input, status.as_str(), start_at, end_at);
    ops_repo::append_audit_tx(
        &mut *c,
        actor,
        "coupon_activity",
        "update",
        "coupon_activity",
        &id.to_string(),
        &before_json,
        &after,
        client_ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 归一化失败统一映射为输入文案。
fn normalize_or_input(
    input: &chargepilot_domain::marketing::ActivityRuleInput,
    now: DateTime<Utc>,
) -> Result<
    (
        DateTime<Utc>,
        DateTime<Utc>,
        chargepilot_domain::marketing::ActivityStatus,
    ),
    CouponError,
> {
    chargepilot_domain::marketing::normalize_activity(input, now).map_err(|_| {
        CouponError::Input(
            "活动规则无效：名称不能为空，门槛为非负分，窗口须为 RFC3339 且结束晚于开始，满减须设门槛，邀请有奖须设邀请人券"
                .into(),
        )
    })
}

/// 审计 after 快照（归一化后的输入）。
fn audit_after(
    input: &chargepilot_domain::marketing::ActivityRuleInput,
    status: &str,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "name": input.name,
        "trigger_type": input.trigger_type,
        "coupon_id": input.coupon_id.to_string(),
        "inviter_coupon_id": input.inviter_coupon_id.map(|v| v.to_string()),
        "threshold_cents": input.threshold_cents,
        "max_grants": input.max_grants,
        "per_user_limit": input.per_user_limit,
        "status": status,
        "start_at": start_at.to_rfc3339(),
        "end_at": end_at.to_rfc3339(),
    })
}

/// 奖励券校验：按模板主键升序 FOR SHARE，
/// 奖励券与邀请人券都须存在且启用。
async fn require_activity_coupons(
    c: &mut MySqlConnection,
    coupon_id: u64,
    inviter_coupon_id: Option<u64>,
) -> Result<(), CouponError> {
    let mut ids = vec![coupon_id];
    if let Some(v) = inviter_coupon_id.filter(|v| *v != 0 && *v != coupon_id) {
        ids.push(v);
    }
    ids.sort_unstable();
    let usable = activity::activity_coupons_share(c, &ids).await?;
    if !usable.get(&coupon_id).copied().unwrap_or(false) {
        return Err(CouponError::Input("活动券不存在或已停用".into()));
    }
    if let Some(v) = inviter_coupon_id.filter(|v| *v != 0)
        && !usable.get(&v).copied().unwrap_or(false)
    {
        return Err(CouponError::Input("邀请人券不存在或已停用".into()));
    }
    Ok(())
}

/// 供 HTTP 层检查活动读权限。
pub fn has_activity_read(actor: &Profile) -> bool {
    actor
        .permissions
        .iter()
        .any(|p| p == "coupon.activity.read")
}
