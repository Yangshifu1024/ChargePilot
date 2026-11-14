//! 活动规则管理与事件触发的幂等发放（旧版 activity.go + activity_grant.go +
//! admin_activity_repo.go + coupon_activity_repo.go 的单体对应物）。
//! 发放锁序沿用旧版：规则主键 → 券模板主键（升序）→ 发放行计数；
//! 模板锁之后不得反向等待用户或钱包。发放号 = sha256(规则+用户+事件键) 前
//! 16 字节 hex，重试不产生新券；唯一键冲突按"已发放"处理。
//! 权限码沿用旧版：coupon.activity.read / coupon.activity.manage，
//! 审计 module/target = "coupon_activity"。

use crate::coupon::CouponError;
use chargepilot_domain::marketing::{
    ActivityTriggerType, CouponStatus, GrantReceipt, GrantSource, GrantStatus, activity_event_id,
    activity_grant_expiry, grant_source_for, invite_eligible, per_user_allowance,
};
use chargepilot_mod_ops::{Profile, repo as ops_repo};
use chrono::{DateTime, Utc};
use sqlx::{Acquire, MySqlConnection, MySqlPool};

/// 事件触发的发放请求（旧版 ActivityEvent）。
#[derive(Debug, Clone)]
pub struct ActivityEvent {
    /// 触发类型（coupon_activity_rule.trigger_type 词表）。
    pub trigger_type: ActivityTriggerType,
    /// 受益用户。
    pub user_id: u64,
    /// 充值请求号或订单号生成的稳定事件键。
    pub event_key: String,
    /// 已确认金额，分；仅门槛规则读取。
    pub amount_cents: i64,
    /// 邀请人主键，非邀请触发不读取。
    pub inviter_id: u64,
    /// 本次判断时间（由调用方提供，保证重试判定一致）。
    pub now: DateTime<Utc>,
}

/// 规则的发放结果，重放不会新增券（旧版 ActivityResult）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivityResult {
    pub rule_id: u64,
    pub coupon_id: u64,
    /// 本次已给邀请人发券，或此前已发过同一事件奖励。
    pub inviter_rewarded: bool,
    /// 受益人发放键已存在（重放）。
    pub already_granted: bool,
}

/// 锁后规则行（旧版 activityRule）：end_at 列 NOT NULL。
#[derive(Debug, Clone)]
struct ActivityRuleRow {
    id: u64,
    trigger_type: String,
    coupon_id: u64,
    inviter_coupon_id: Option<u64>,
    /// 锁读复查在 SQL 条件内完成；字段保留与行结构对齐，供后续读路径复用。
    #[allow(dead_code)]
    threshold_cents: i64,
    max_grants: i32,
    per_user_limit: i32,
    #[allow(dead_code)]
    status: String,
    #[allow(dead_code)]
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
}

/// 锁后模板行（旧版 activityCoupon）：只含发放判定所需字段。
#[derive(Debug, Clone)]
struct ActivityCouponRow {
    #[allow(dead_code)]
    id: u64,
    /// coupon.status：券模板启停（与规则 status 同名不同表不同义）。
    status: CouponStatus,
    valid_hours: u32,
    start_at: Option<DateTime<Utc>>,
    end_at: Option<DateTime<Utc>>,
}

type RuleTup = (
    u64,
    String,
    u64,
    Option<u64>,
    i64,
    i32,
    i32,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

type CouponTup = (
    u64,
    CouponStatus,
    u32,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// 候选规则筛选（无锁，旧版 activeRules 第一段）：触发类型 + 启用 + 窗口 + 门槛。
/// 阈值规则要求门槛为正且不超过已确认金额；候选与锁读共用同一条件。
async fn candidate_rule_ids(
    c: &mut MySqlConnection,
    event: &ActivityEvent,
) -> Result<Vec<u64>, CouponError> {
    let has_threshold =
        event.trigger_type == chargepilot_domain::marketing::ActivityTriggerType::ThresholdRedeem;
    let sql = format!(
        "SELECT `id` FROM `coupon_activity_rule` WHERE `trigger_type` = ? AND `status` = 'active' \
         AND `deleted_at` IS NULL AND `start_at` <= ? AND `end_at` > ?{} ORDER BY `id`",
        if has_threshold {
            " AND `threshold_cents` > 0 AND `threshold_cents` <= ?"
        } else {
            ""
        }
    );
    let mut q = sqlx::query_scalar::<_, u64>(&sql)
        .bind(event.trigger_type)
        .bind(event.now)
        .bind(event.now);
    if has_threshold {
        q = q.bind(event.amount_cents);
    }
    Ok(q.fetch_all(c).await?)
}

/// 锁读规则并复查（旧版 activeRules 第二段）：候选 ID FOR UPDATE，条件与候选一致。
async fn lock_rules(
    c: &mut MySqlConnection,
    event: &ActivityEvent,
    ids: &[u64],
) -> Result<Vec<ActivityRuleRow>, CouponError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let has_threshold =
        event.trigger_type == chargepilot_domain::marketing::ActivityTriggerType::ThresholdRedeem;
    let sql = format!(
        "SELECT `id`, `trigger_type`, `coupon_id`, `inviter_coupon_id`, `threshold_cents`, \
         `max_grants`, `per_user_limit`, `status`, `start_at`, `end_at` \
         FROM `coupon_activity_rule` WHERE `id` IN ({placeholders}) AND `trigger_type` = ? \
         AND `status` = 'active' AND `deleted_at` IS NULL AND `start_at` <= ? AND `end_at` > ?{} \
         ORDER BY `id` FOR UPDATE",
        if has_threshold {
            " AND `threshold_cents` > 0 AND `threshold_cents` <= ?"
        } else {
            ""
        }
    );
    let mut q = sqlx::query_as::<_, RuleTup>(&sql);
    for id in ids {
        q = q.bind(*id as i64);
    }
    q = q.bind(event.trigger_type).bind(event.now).bind(event.now);
    if has_threshold {
        q = q.bind(event.amount_cents);
    }
    let rows = q.fetch_all(&mut *c).await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                trigger_type,
                coupon_id,
                inviter_coupon_id,
                threshold_cents,
                max_grants,
                per_user_limit,
                status,
                start_at,
                end_at,
            )| ActivityRuleRow {
                id,
                trigger_type,
                coupon_id,
                inviter_coupon_id,
                threshold_cents,
                max_grants,
                per_user_limit,
                status,
                start_at,
                end_at,
            },
        )
        .collect())
}

/// 在规则锁后按模板主键升序加写锁（旧版 lockActivityCoupons）；缺失模板由
/// 发放逻辑判为不适用。
async fn lock_coupons(
    c: &mut MySqlConnection,
    ids: &[u64],
) -> Result<std::collections::HashMap<u64, ActivityCouponRow>, CouponError> {
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT `id`, `status`, `valid_hours`, `start_at`, `end_at` FROM `coupon` \
         WHERE `id` IN ({placeholders}) AND `deleted_at` IS NULL ORDER BY `id` FOR UPDATE"
    );
    let mut q = sqlx::query_as::<_, CouponTup>(&sql);
    for id in ids {
        q = q.bind(*id as i64);
    }
    let rows = q.fetch_all(&mut *c).await?;
    for (id, status, valid_hours, start_at, end_at) in rows {
        out.insert(
            id,
            ActivityCouponRow {
                id,
                status,
                valid_hours,
                start_at,
                end_at,
            },
        );
    }
    Ok(out)
}

/// 模板状态与有效时间检查（旧版 usableActivityCoupon）；缺失模板不可用。
fn usable_coupon(coupon: &ActivityCouponRow, now: DateTime<Utc>) -> bool {
    coupon.status == CouponStatus::Active
        && coupon.start_at.is_none_or(|s| now >= s)
        && coupon.end_at.is_none_or(|e| now < e)
}

/// 发放有效期：有效小时、模板结束、活动结束三者最早（旧版 activityExpiry）。
fn activity_expiry(
    rule: &ActivityRuleRow,
    coupon: &ActivityCouponRow,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    activity_grant_expiry(now, coupon.valid_hours as i32, coupon.end_at, rule.end_at)
}

/// 邀请人是否已建立（旧版 isEstablishedUser）：有已完成充电或已到账充值。
/// 只读相关表，不加锁。
async fn is_established_user(c: &mut MySqlConnection, user_id: u64) -> anyhow::Result<bool> {
    let orders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_order` WHERE `user_id` = ? AND `status` = 'completed' \
         AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_one(&mut *c)
    .await?;
    if orders > 0 {
        return Ok(true);
    }
    let recharges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `payment_order` WHERE `user_id` = ? AND `biz_type` = 'wallet_recharge' \
         AND `status` = 'paid' AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_one(&mut *c)
    .await?;
    Ok(recharges > 0)
}

/// 同券模板和发放来源的已发数量（旧版 countRuleGrants，模板锁内当前读）。
async fn count_rule_grants(
    c: &mut MySqlConnection,
    rule: &ActivityRuleRow,
) -> Result<i64, CouponError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `coupon_grant` WHERE `coupon_id` = ? AND `grant_source` = ? \
         AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(rule.coupon_id as i64)
    .bind(grant_source_for(&rule.trigger_type))
    .fetch_one(&mut *c)
    .await?;
    Ok(n)
}

/// 用户对该券的累计发放量，各来源共同计入单人额度（旧版 countUserGrants）。
async fn count_user_grants(
    c: &mut MySqlConnection,
    coupon_id: u64,
    user_id: u64,
) -> Result<i64, CouponError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `coupon_grant` WHERE `coupon_id` = ? AND `user_id` = ? \
         AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .fetch_one(&mut *c)
    .await?;
    Ok(n)
}

/// 指定用户和事件的邀请奖励是否已保存（旧版 activityGrantExists）。
async fn activity_grant_exists(
    c: &mut MySqlConnection,
    event_id: &str,
    user_id: u64,
) -> Result<bool, CouponError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `coupon_grant` WHERE `source_event_id` = ? AND `user_id` = ? \
         AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(event_id)
    .bind(user_id as i64)
    .fetch_one(&mut *c)
    .await?;
    Ok(n > 0)
}

/// 发放回执查询（旧版 findGrantReceipt 的活动复用：request_id 即发放号）。
async fn find_receipt(
    c: &mut MySqlConnection,
    event_id: &str,
) -> Result<Option<GrantReceipt>, CouponError> {
    let row: Option<(u64, u64, u64)> = sqlx::query_as(
        "SELECT `coupon_id`, `user_id`, `coupon_grant_id` FROM `coupon_grant_request` \
         WHERE `request_id` = ? FOR SHARE",
    )
    .bind(event_id)
    .fetch_optional(c)
    .await?;
    Ok(row.map(|(coupon_id, user_id, grant_id)| GrantReceipt {
        request_id: event_id.to_string(),
        coupon_id,
        user_id,
        coupon_grant_id: grant_id,
    }))
}

/// 保存新券；唯一键重复返回已发放（旧版 saveActivityGrant）。
/// receipt=true 时把发放号与发放 ID 关联写入 coupon_grant_request（重试判重依据）。
async fn save_grant(
    c: &mut MySqlConnection,
    coupon_id: u64,
    user_id: u64,
    source: &str,
    expires_at: DateTime<Utc>,
    event_id: &str,
    receipt: bool,
) -> Result<bool, CouponError> {
    let inserted = sqlx::query(
        "INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, \
         `expired_at`, `source_event_id`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .bind(source)
    .bind(GrantStatus::Unused)
    .bind(expires_at)
    .bind(event_id)
    .execute(&mut *c)
    .await;
    let result = match inserted {
        Ok(r) => r,
        Err(e) => {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23000")
            {
                return Ok(true);
            }
            return Err(e.into());
        }
    };
    if !receipt {
        return Ok(false);
    }
    let grant_id = result.last_insert_id();
    // 请求回执唯一键冲突（并发重放）同样按已发放处理。
    match sqlx::query(
        "INSERT INTO `coupon_grant_request` (`request_id`, `coupon_id`, `user_id`, `coupon_grant_id`) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(event_id)
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .bind(grant_id as i64)
    .execute(&mut *c)
    .await
    {
        Ok(_) => Ok(false),
        Err(e) => {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23000") {
                    return Ok(true);
                }
            Err(e.into())
        }
    }
}

/// 事件触发的幂等发放（旧版 ApplyInTx）：候选 → 锁读规则 → 模板锁 → 逐规则发放。
/// 使用调用方事务（充值回调/订单完成的资金事务内调用），失败整体回滚；
/// 条件不满足的规则逐条跳过（ErrActivityNotApplicable 语义），不中断其他规则。
pub async fn apply_in_tx(
    c: &mut MySqlConnection,
    event: &ActivityEvent,
) -> Result<Vec<ActivityResult>, CouponError> {
    if event.user_id == 0 || event.event_key.is_empty() {
        return Ok(vec![]);
    }
    let rule_ids = candidate_rule_ids(c, event).await?;
    let rules = lock_rules(c, event, &rule_ids).await?;
    let mut coupon_ids = Vec::with_capacity(rules.len() * 2);
    for rule in &rules {
        coupon_ids.push(rule.coupon_id);
        if let Some(id) = rule.inviter_coupon_id.filter(|v| *v > 0) {
            coupon_ids.push(id);
        }
    }
    let coupons = lock_coupons(c, &coupon_ids).await?;
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let results = apply_in_tx(&mut *c, event).await?;
    tx.commit().await?;
    Ok(results)
}

/// 单规则发放判定与写入（旧版 grantFromRule）。返回 None 表示规则不适用。
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
    if let Some(receipt) = find_receipt(c, &event_id).await? {
        if receipt.user_id != event.user_id {
            return Err(CouponError::Conflict("发放请求与首次请求不一致".into()));
        }
        out.coupon_id = receipt.coupon_id;
        out.already_granted = true;
        if chargepilot_domain::marketing::ActivityTriggerType::parse(rule.trigger_type.as_str())
            == Ok(chargepilot_domain::marketing::ActivityTriggerType::InviteReward)
            && event.inviter_id != 0
        {
            let inviter_event_id =
                activity_event_id(rule.id, event.inviter_id, &event.event_key, &["inviter"]);
            out.inviter_rewarded =
                activity_grant_exists(c, &inviter_event_id, event.inviter_id).await?;
        }
        return Ok(Some(out));
    }
    let Some(coupon) = coupons.get(&rule.coupon_id) else {
        return Ok(None);
    };
    if !usable_coupon(coupon, event.now) {
        return Ok(None);
    }
    if chargepilot_domain::marketing::ActivityTriggerType::parse(rule.trigger_type.as_str())
        == Ok(chargepilot_domain::marketing::ActivityTriggerType::InviteReward)
    {
        if !invite_eligible(&rule.trigger_type, event.inviter_id, event.user_id) {
            return Ok(None);
        }
        if !is_established_user(c, event.inviter_id).await? {
            return Ok(None);
        }
    }
    if rule.max_grants > 0 {
        let n = count_rule_grants(c, rule).await?;
        if n >= rule.max_grants as i64 {
            return Ok(None);
        }
    }
    let allowance = per_user_allowance(rule.per_user_limit);
    let n = count_user_grants(c, rule.coupon_id, event.user_id).await?;
    if n >= allowance {
        return Ok(None);
    }
    let source = grant_source_for(&rule.trigger_type);
    let expires_at = activity_expiry(rule, coupon, event.now);
    out.already_granted = save_grant(
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
    if chargepilot_domain::marketing::ActivityTriggerType::parse(rule.trigger_type.as_str())
        == Ok(chargepilot_domain::marketing::ActivityTriggerType::InviteReward)
        && let Some(inviter_coupon_id) = rule.inviter_coupon_id.filter(|v| *v > 0)
    {
        let Some(inviter_coupon) = coupons.get(&inviter_coupon_id) else {
            return Ok(Some(out));
        };
        if !usable_coupon(inviter_coupon, event.now) {
            return Ok(Some(out));
        }
        let n = count_user_grants(c, inviter_coupon_id, event.inviter_id).await?;
        if n >= allowance {
            return Ok(Some(out));
        }
        let inviter_event_id =
            activity_event_id(rule.id, event.inviter_id, &event.event_key, &["inviter"]);
        let expires_at = activity_expiry(rule, inviter_coupon, event.now);
        let _ = save_grant(
            c,
            inviter_coupon_id,
            event.inviter_id,
            GrantSource::InviteReward.as_str(),
            expires_at,
            &inviter_event_id,
            false,
        )
        .await?;
        out.inviter_rewarded = true;
    }
    Ok(Some(out))
}

// ---------- 后台活动规则管理（旧版 AdminActivityRepo + ActivityService） ----------

/// 活动规则列表行（旧版 AdminActivityRuleRow；JSON 键名一致）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivityRow {
    pub id: u64,
    pub name: String,
    pub trigger_type: String,
    pub coupon_id: u64,
    pub coupon_name: String,
    pub inviter_coupon_id: Option<u64>,
    pub threshold_cents: i64,
    pub max_grants: i32,
    pub per_user_limit: i32,
    pub status: String,
    pub start_at: DateTime<Utc>,
    pub end_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// 活动/邀请来源的累计发放份数（引用同一模板的规则共享计数）。
    pub granted_count: i64,
}

type ActivityTup = (
    u64,
    String,
    String,
    u64,
    String,
    Option<u64>,
    i64,
    i32,
    i32,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

/// LIKE 模式转义（旧版 dbutil.LikePattern：以 ! 为转义符）。
fn like_pattern(keyword: &str) -> String {
    let mut out = String::from("%");
    for ch in keyword.chars() {
        match ch {
            '!' | '%' | '_' => {
                out.push('!');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('%');
    out
}

/// 按状态和名称分页查询规则，补充各券模板实际发放量（旧版 ListActivityRules）。
/// 状态筛选项已按两值白名单校验后原样作为 WHERE 参数。
pub async fn list_activity_rules(
    pool: &MySqlPool,
    actor: &Profile,
    status: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<ActivityRow>, i64), CouponError> {
    if !actor
        .permissions
        .iter()
        .any(|p| p == "coupon.activity.read")
    {
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
    let mut where_sql = String::from("r.`deleted_at` IS NULL");
    if !status.is_empty() {
        where_sql.push_str(" AND r.`status` = ?");
    }
    if !keyword.is_empty() {
        where_sql.push_str(" AND r.`name` LIKE ? ESCAPE '!'");
    }
    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM `coupon_activity_rule` AS r WHERE {where_sql}");
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        if !status.is_empty() {
            q = q.bind(status);
        }
        if !keyword.is_empty() {
            q = q.bind(like_pattern(keyword));
        }
        q.fetch_one(&mut *c).await?
    };
    let rows: Vec<ActivityTup> = {
        let sql = format!(
            "SELECT r.`id`, r.`name`, r.`trigger_type`, r.`coupon_id`, c.`name`, \
             r.`inviter_coupon_id`, r.`threshold_cents`, r.`max_grants`, r.`per_user_limit`, \
             r.`status`, r.`start_at`, r.`end_at`, r.`created_at` \
             FROM `coupon_activity_rule` AS r LEFT JOIN `coupon` AS c ON c.`id` = r.`coupon_id` \
             WHERE {where_sql} ORDER BY r.`id` DESC LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query_as::<_, ActivityTup>(&sql);
        if !status.is_empty() {
            q = q.bind(status);
        }
        if !keyword.is_empty() {
            q = q.bind(like_pattern(keyword));
        }
        q = q.bind(page_size).bind((page - 1) * page_size);
        q.fetch_all(&mut *c).await?
    };
    // 批量统计活动/邀请来源发放量（仅这两种来源计入活动口径）。
    let coupon_ids: Vec<u64> = rows.iter().map(|r| r.3).collect();
    let mut counts = std::collections::HashMap::new();
    if !coupon_ids.is_empty() {
        let placeholders = coupon_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT `coupon_id`, COUNT(*) FROM `coupon_grant` WHERE `coupon_id` IN ({placeholders}) \
             AND `grant_source` IN ('activity','invite_reward') AND `deleted_at` IS NULL GROUP BY `coupon_id`"
        );
        let mut q = sqlx::query_as::<_, (u64, i64)>(&sql);
        for id in &coupon_ids {
            q = q.bind(*id as i64);
        }
        let pairs = q.fetch_all(&mut *c).await?;
        for (coupon_id, total) in pairs {
            counts.insert(coupon_id, total);
        }
    }
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

/// 创建活动规则并同事务审计（旧版 Create）：奖励券须存在且启用（FOR SHARE），
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    require_activity_coupons(&mut *c, input.coupon_id, input.inviter_coupon_id).await?;
    let result = sqlx::query(
        "INSERT INTO `coupon_activity_rule` (`name`, `trigger_type`, `coupon_id`, \
         `inviter_coupon_id`, `threshold_cents`, `max_grants`, `per_user_limit`, `status`, \
         `start_at`, `end_at`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&input.name)
    .bind(&input.trigger_type)
    .bind(input.coupon_id as i64)
    .bind(input.inviter_coupon_id.map(|v| v as i64))
    .bind(input.threshold_cents)
    .bind(input.max_grants)
    .bind(input.per_user_limit)
    .bind(status)
    .bind(start_at)
    .bind(end_at)
    .execute(&mut *c)
    .await?;
    let id = result.last_insert_id();
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

/// 更新活动规则（旧版 Update）：触发类型保持不变；先锁规则再校验奖励券，
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 锁定读旧规则（触发类型不可变；同时作为存在性判定）。
    let before: Option<(String, String)> = sqlx::query_as(
        "SELECT `name`, `trigger_type` FROM `coupon_activity_rule` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((before_name, before_trigger)) = before else {
        return Err(CouponError::NotFound);
    };
    if before_trigger != input.trigger_type {
        return Err(CouponError::Input("活动触发类型不可修改".into()));
    }
    require_activity_coupons(&mut *c, input.coupon_id, input.inviter_coupon_id).await?;
    let changed = sqlx::query(
        "UPDATE `coupon_activity_rule` SET `name` = ?, `coupon_id` = ?, `inviter_coupon_id` = ?, \
         `threshold_cents` = ?, `max_grants` = ?, `per_user_limit` = ?, `status` = ?, \
         `start_at` = ?, `end_at` = ? WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(&input.name)
    .bind(input.coupon_id as i64)
    .bind(input.inviter_coupon_id.map(|v| v as i64))
    .bind(input.threshold_cents)
    .bind(input.max_grants)
    .bind(input.per_user_limit)
    .bind(status)
    .bind(start_at)
    .bind(end_at)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    if changed.rows_affected() != 1 {
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

/// 归一化失败统一映射为旧版输入文案。
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

/// 审计 after 快照（归一化后的输入，键名与旧版 ActivityInput JSON 一致）。
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

/// 奖励券校验（旧版 requireActivityCoupons）：按模板主键升序 FOR SHARE，
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
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT `id`, `status` FROM `coupon` WHERE `id` IN ({placeholders}) \
         AND `deleted_at` IS NULL ORDER BY `id` FOR SHARE"
    );
    let mut q = sqlx::query_as::<_, (u64, CouponStatus)>(&sql);
    for id in &ids {
        q = q.bind(*id as i64);
    }
    let rows = q.fetch_all(&mut *c).await?;
    let usable: std::collections::HashMap<u64, bool> = rows
        .into_iter()
        .map(|(id, status)| (id, status == CouponStatus::Active))
        .collect();
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

/// 供 HTTP 层检查活动读权限（旧版 List 在服务内核对 coupon.activity.read）。
pub fn has_activity_read(actor: &Profile) -> bool {
    actor
        .permissions
        .iter()
        .any(|p| p == "coupon.activity.read")
}
