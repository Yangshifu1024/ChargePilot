//! coupon_activity_rule / coupon_grant 活动链表访问（蓝图第五章 repo 层）。

use crate::error::CouponError;
use chargepilot_domain::marketing::{CouponStatus, GrantReceipt, GrantStatus, grant_source_for};
use chrono::{DateTime, Utc};
use sqlx::MySqlConnection;

/// 事件触发的发放请求（跨层共享输入）。
#[derive(Debug, Clone)]
pub struct ActivityEvent {
    /// 触发类型（coupon_activity_rule.trigger_type 词表）。
    pub trigger_type: chargepilot_domain::marketing::ActivityTriggerType,
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

/// 锁后规则行：end_at 列 NOT NULL。
#[derive(Debug, Clone)]
pub struct ActivityRuleRow {
    pub id: u64,
    pub trigger_type: String,
    pub coupon_id: u64,
    pub inviter_coupon_id: Option<u64>,
    pub threshold_cents: i64,
    pub max_grants: i32,
    pub per_user_limit: i32,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
}

/// 锁后模板行：只含发放判定所需字段。
#[derive(Debug, Clone)]
pub struct ActivityCouponRow {
    pub id: u64,
    /// coupon.status：券模板启停（与规则 status 同名不同表不同义）。
    pub status: CouponStatus,
    pub valid_hours: u32,
    pub start_at: Option<DateTime<Utc>>,
    pub end_at: Option<DateTime<Utc>>,
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

/// 候选规则筛选（无锁）：触发类型 + 启用 + 窗口 + 门槛。
/// 阈值规则要求门槛为正且不超过已确认金额；候选与锁读共用同一条件。
pub async fn candidate_rule_ids(
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
    Ok(q.fetch_all(&mut *c).await?)
}

/// 锁读规则并复查：候选 ID FOR UPDATE，条件与候选一致。
pub async fn lock_rules(
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
                _status,
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
                start_at,
                end_at,
            },
        )
        .collect())
}

/// 在规则锁后按模板主键升序加写锁；缺失模板由发放逻辑判为不适用。
pub async fn lock_coupons(
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

/// 邀请人是否已建立：有已完成充电或已到账充值。只读相关表，不加锁。
pub async fn is_established_user(c: &mut MySqlConnection, user_id: u64) -> anyhow::Result<bool> {
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

/// 同券模板和发放来源的已发数量（模板锁内当前读）。
pub async fn count_rule_grants(
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

/// 用户对该券的累计发放量，各来源共同计入单人额度。
pub async fn count_user_grants(
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

/// 指定用户和事件的邀请奖励是否已保存。
pub async fn activity_grant_exists(
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

/// 发放回执查询（request_id 即发放号；FOR SHARE）。
pub async fn find_receipt(
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

/// 保存新券；唯一键重复返回已发放。
/// receipt=true 时把发放号与发放 ID 关联写入 coupon_grant_request（重试判重依据）。
pub async fn save_grant(
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
                && db.code().as_deref() == Some("23000")
            {
                return Ok(true);
            }
            Err(e.into())
        }
    }
}

// ---------- 后台活动规则管理 ----------

/// 活动规则列表行（JSON 键名与契约一致）。
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

/// LIKE 模式转义（以 ! 为转义符）。
pub fn like_pattern(keyword: &str) -> String {
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

/// 规则计数与分页行（状态/关键词过滤已由调用方校验）。
pub async fn rule_page(
    c: &mut MySqlConnection,
    status: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<ActivityTup>, i64), CouponError> {
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
    Ok((rows, total))
}

/// 批量统计活动/邀请来源发放量（仅这两种来源计入活动口径）。
pub async fn grant_counts_by_coupon(
    c: &mut MySqlConnection,
    coupon_ids: &[u64],
) -> Result<std::collections::HashMap<u64, i64>, CouponError> {
    let mut counts = std::collections::HashMap::new();
    if coupon_ids.is_empty() {
        return Ok(counts);
    }
    let placeholders = coupon_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT `coupon_id`, COUNT(*) FROM `coupon_grant` WHERE `coupon_id` IN ({placeholders}) \
         AND `grant_source` IN ('activity','invite_reward') AND `deleted_at` IS NULL GROUP BY `coupon_id`"
    );
    let mut q = sqlx::query_as::<_, (u64, i64)>(&sql);
    for id in coupon_ids {
        q = q.bind(*id as i64);
    }
    for (coupon_id, total) in q.fetch_all(&mut *c).await? {
        counts.insert(coupon_id, total);
    }
    Ok(counts)
}

/// 创建活动规则，返回自增主键（校验由调用方完成）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_rule_tx(
    c: &mut MySqlConnection,
    name: &str,
    trigger_type: &str,
    coupon_id: u64,
    inviter_coupon_id: Option<u64>,
    threshold_cents: i64,
    max_grants: i32,
    per_user_limit: i32,
    status: &str,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> Result<u64, CouponError> {
    let result = sqlx::query(
        "INSERT INTO `coupon_activity_rule` (`name`, `trigger_type`, `coupon_id`, \
         `inviter_coupon_id`, `threshold_cents`, `max_grants`, `per_user_limit`, `status`, \
         `start_at`, `end_at`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(name)
    .bind(trigger_type)
    .bind(coupon_id as i64)
    .bind(inviter_coupon_id.map(|v| v as i64))
    .bind(threshold_cents)
    .bind(max_grants)
    .bind(per_user_limit)
    .bind(status)
    .bind(start_at)
    .bind(end_at)
    .execute(&mut *c)
    .await?;
    Ok(result.last_insert_id())
}

/// 锁定读旧规则（触发类型不可变；同时作为存在性判定）。
pub async fn lock_rule_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<(String, String)>, CouponError> {
    sqlx::query_as(
        "SELECT `name`, `trigger_type` FROM `coupon_activity_rule` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await
    .map_err(CouponError::from)
}

/// 更新活动规则（触发类型保持不变由调用方核对）；返回是否命中行。
#[allow(clippy::too_many_arguments)]
pub async fn update_rule_tx(
    c: &mut MySqlConnection,
    id: u64,
    name: &str,
    coupon_id: u64,
    inviter_coupon_id: Option<u64>,
    threshold_cents: i64,
    max_grants: i32,
    per_user_limit: i32,
    status: &str,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> Result<bool, CouponError> {
    let changed = sqlx::query(
        "UPDATE `coupon_activity_rule` SET `name` = ?, `coupon_id` = ?, `inviter_coupon_id` = ?, \
         `threshold_cents` = ?, `max_grants` = ?, `per_user_limit` = ?, `status` = ?, \
         `start_at` = ?, `end_at` = ? WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(name)
    .bind(coupon_id as i64)
    .bind(inviter_coupon_id.map(|v| v as i64))
    .bind(threshold_cents)
    .bind(max_grants)
    .bind(per_user_limit)
    .bind(status)
    .bind(start_at)
    .bind(end_at)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// 奖励券可用性读取（按模板主键升序 FOR SHARE）。
pub async fn activity_coupons_share(
    c: &mut MySqlConnection,
    ids: &[u64],
) -> Result<std::collections::HashMap<u64, bool>, CouponError> {
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT `id`, `status` FROM `coupon` WHERE `id` IN ({placeholders}) \
         AND `deleted_at` IS NULL ORDER BY `id` FOR SHARE"
    );
    let mut q = sqlx::query_as::<_, (u64, CouponStatus)>(&sql);
    for id in ids {
        q = q.bind(*id as i64);
    }
    let rows = q.fetch_all(&mut *c).await?;
    Ok(rows
        .into_iter()
        .map(|(id, status)| (id, status == CouponStatus::Active))
        .collect())
}
