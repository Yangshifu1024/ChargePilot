//! 优惠券模板与人工发放（旧版 coupon.go + admin_coupon_repo.go 的单体对应物）。
//! 权限码沿用旧版：coupon.read / coupon.create / coupon.update / coupon.grant；
//! 审计 module/target 均为 "coupon"（旧版 AppendAuditInTx 以 Target 作 module）。
//! 发放锁序沿用旧版：请求回执（读）→ 用户行 → 券模板行 → 发放行计数；
//! 模板锁之后不得反向等待用户或钱包。coupon 主键沿用 AUTO_INCREMENT
//! （旧版 CreateCoupon 由库自增并回填响应），不走号段分配器。

use chargepilot_domain::marketing::GrantStatus;
use chargepilot_mod_identity::repo as identity_repo;
use chargepilot_mod_ops::{Profile, repo as ops_repo};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

/// 券域错误（旧版 InputError / ErrCouponGrant / operations.ErrForbidden /
/// gorm.ErrRecordNotFound 的映射源；HTTP 层按变体转码值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouponError {
    /// 请求内容不合法（400/1005，文案透出）。
    Input(String),
    /// 权限不足（403/1003）。
    Forbidden,
    /// 发放条件不满足或请求冲突（409/2009，文案透出）。
    Conflict(String),
    /// 模板不存在（404/1004）。
    NotFound,
    /// 存储错误（503/5003）。
    Storage(String),
}

impl std::fmt::Display for CouponError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CouponError::Input(m) => f.write_str(m),
            CouponError::Forbidden => f.write_str("无权执行此操作"),
            CouponError::Conflict(m) => f.write_str(m),
            CouponError::NotFound => f.write_str("记录不存在或已删除"),
            CouponError::Storage(m) => write!(f, "优惠券存储错误: {m}"),
        }
    }
}

impl std::error::Error for CouponError {}

impl From<sqlx::Error> for CouponError {
    fn from(e: sqlx::Error) -> Self {
        CouponError::Storage(e.to_string())
    }
}

impl From<anyhow::Error> for CouponError {
    fn from(e: anyhow::Error) -> Self {
        CouponError::Storage(e.to_string())
    }
}

/// 券模板行（旧版 AdminCouponRow / Coupon；JSON 键名与旧版一致）。
/// discount_percent 为应付比例百分数（80 表示八折），两位小数；
/// amount/percentage/time_free 三型的优惠列互斥，读取侧不做替换或降级。
#[derive(Debug, Clone, Serialize)]
pub struct CouponRow {
    pub id: u64,
    pub name: String,
    pub discount_type: String,
    pub discount_value_cents: Option<i64>,
    pub discount_percent: Option<f64>,
    pub free_minutes: Option<u32>,
    pub min_charge_cents: i64,
    pub valid_hours: u32,
    pub total_quota: u32,
    pub per_user_quota: u32,
    pub status: String,
    pub start_at: Option<DateTime<Utc>>,
    pub end_at: Option<DateTime<Utc>>,
}

/// coupon 表查询投影：percent 经 CAST AS CHAR（decimal(5,2) 不直读浮点）。
type CouponTup = (
    u64,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<u32>,
    i64,
    u32,
    u32,
    u32,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn coupon_from(t: CouponTup) -> CouponRow {
    let (
        id,
        name,
        discount_type,
        discount_value_cents,
        percent_raw,
        free_minutes,
        min_charge_cents,
        valid_hours,
        total_quota,
        per_user_quota,
        status,
        start_at,
        end_at,
    ) = t;
    CouponRow {
        id,
        name,
        discount_type,
        discount_value_cents,
        discount_percent: percent_raw.and_then(|s| s.parse().ok()),
        free_minutes,
        min_charge_cents,
        valid_hours,
        total_quota,
        per_user_quota,
        status,
        start_at,
        end_at,
    }
}

const COUPON_COLUMNS: &str = "`id`, `name`, `discount_type`, `discount_value_cents`, \
     CAST(`discount_percent` AS CHAR) AS `discount_percent`, `free_minutes`, \
     `min_charge_cents`, `valid_hours`, `total_quota`, `per_user_quota`, `status`, \
     `start_at`, `end_at`";

/// 模板更新输入：只允许修改名称和启停状态，不改变优惠和发放上限（旧版 CouponUpdate）。
#[derive(Debug, Clone)]
pub struct CouponUpdate {
    pub name: String,
    pub status: String,
}

/// 人工发放输入（旧版 GrantInput）。
#[derive(Debug, Clone)]
pub struct GrantInput {
    /// UUID 请求号，重试沿用同一号。
    pub request_id: String,
    /// 目标用户主键。
    pub user_id: u64,
}

/// 发放结果：发放主键和请求号，重放不生成新编号（旧版 GrantResult）。
#[derive(Debug, Clone, Serialize)]
pub struct GrantResult {
    pub coupon_grant_id: u64,
    pub request_id: String,
}

/// 发放统计（旧版 AdminCouponStats）：未用但已到期计为 expired。
#[derive(Debug, Clone, Serialize)]
pub struct CouponStats {
    pub total_quota: u32,
    pub granted_count: i64,
    pub unused_count: i64,
    pub used_count: i64,
    pub expired_count: i64,
    pub usage_rate: f64,
}

/// 实时权限判定（旧版 actor.HasPermission）。
fn has_permission(profile: &Profile, code: &str) -> bool {
    profile.permissions.iter().any(|p| p == code)
}

/// 自由文本校验：去首尾空白后非空且按 rune 计数不超限（旧版 operations.ValidText）。
fn valid_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max
}

/// UUID 请求号校验（旧版 operations.ValidRequestID：8-4-4-4-12 十六进制）。
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

/// 优惠参数三选一互斥校验（旧版 validDiscount）：
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
            // 严格 (0,100) 开区间，且两位小数内可精确表示（旧版 Round(2) 后相等）。
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

/// 读取未删除模板行；locked=true 时加 FOR UPDATE（发放与更新的锁点）。
async fn load_coupon(
    c: &mut sqlx::MySqlConnection,
    id: u64,
    locked: bool,
) -> Result<Option<CouponRow>, CouponError> {
    let sql = format!(
        "SELECT {COUPON_COLUMNS} FROM `coupon` WHERE `id` = ? AND `deleted_at` IS NULL{}",
        if locked { " FOR UPDATE" } else { "" }
    );
    let row: Option<CouponTup> = sqlx::query_as(&sql)
        .bind(id as i64)
        .fetch_optional(&mut *c)
        .await?;
    Ok(row.map(coupon_from))
}

/// 后台模板列表：核对 coupon.read 后倒序读取未删除模板（旧版 List）。
pub async fn list_coupons(
    pool: &MySqlPool,
    actor: &Profile,
) -> Result<Vec<CouponRow>, CouponError> {
    if !has_permission(actor, "coupon.read") {
        return Err(CouponError::Forbidden);
    }
    let rows: Vec<CouponTup> = sqlx::query_as(&format!(
        "SELECT {COUPON_COLUMNS} FROM `coupon` WHERE `deleted_at` IS NULL ORDER BY `id` DESC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(coupon_from).collect())
}

/// 创建启用模板并同事务保存审计（旧版 Create）：客户端主键必须为 0，
/// 状态强制 active（忽略提交值），优惠参数三选一互斥。
pub async fn create_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    in_row: CouponRow,
) -> Result<CouponRow, CouponError> {
    if !has_permission(actor, "coupon.create") {
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let result = sqlx::query(
        "INSERT INTO `coupon` (`name`, `discount_type`, `discount_value_cents`, \
         `discount_percent`, `free_minutes`, `min_charge_cents`, `valid_hours`, \
         `total_quota`, `per_user_quota`, `status`, `start_at`, `end_at`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&in_row.name)
    .bind(&in_row.discount_type)
    .bind(in_row.discount_value_cents)
    .bind(in_row.discount_percent)
    .bind(in_row.free_minutes)
    .bind(in_row.min_charge_cents)
    .bind(in_row.valid_hours)
    .bind(in_row.total_quota)
    .bind(in_row.per_user_quota)
    .bind(in_row.start_at)
    .bind(in_row.end_at)
    .execute(&mut *c)
    .await?;
    let id = result.last_insert_id();
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

/// 更新模板名称与启停状态并同事务保存审计（旧版 Update）：
/// 锁定读取旧值，优惠与额度不可修改。
pub async fn update_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    id: u64,
    update: CouponUpdate,
) -> Result<CouponUpdate, CouponError> {
    if !has_permission(actor, "coupon.update") {
        return Err(CouponError::Forbidden);
    }
    if id == 0
        || !valid_text(&update.name, 128)
        || chargepilot_domain::marketing::CouponStatus::parse(update.status.as_str()).is_err()
    {
        return Err(CouponError::Input("名称或状态无效".into()));
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(before) = load_coupon(&mut *c, id, true).await? else {
        return Err(CouponError::NotFound);
    };
    sqlx::query("UPDATE `coupon` SET `name` = ?, `status` = ? WHERE `id` = ?")
        .bind(&update.name)
        .bind(&update.status)
        .bind(id as i64)
        .execute(&mut *c)
        .await?;
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

/// 发放统计（旧版 Stats）：unused 但已到期计为 expired；
/// SUM 布尔结果必须 CAST AS SIGNED（MySQL 对布尔聚合返回 DECIMAL）。
pub async fn coupon_stats(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
) -> Result<CouponStats, CouponError> {
    if !has_permission(actor, "coupon.read") {
        return Err(CouponError::Forbidden);
    }
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(coupon) = load_coupon(&mut *c, id, false).await? else {
        return Err(CouponError::NotFound);
    };
    let (granted, unused, used, expired): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), \
         COALESCE(CAST(SUM(`status`='unused' AND `expired_at`>UTC_TIMESTAMP(3)) AS SIGNED),0), \
         COALESCE(CAST(SUM(`status`='used') AS SIGNED),0), \
         COALESCE(CAST(SUM(`status`='expired' OR (`status`='unused' AND `expired_at`<=UTC_TIMESTAMP(3))) AS SIGNED),0) \
         FROM `coupon_grant` WHERE `coupon_id` = ?",
    )
    .bind(id as i64)
    .fetch_one(&mut *c)
    .await?;
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

/// 人工发放（旧版 Grant）：核对 coupon.grant 权限后向指定用户发券，
/// 发放与审计在同一事务提交；重试沿用请求号。发放条件不满足按冲突处理。
pub async fn grant_coupon(
    pool: &MySqlPool,
    actor: &Profile,
    client_ip: &str,
    request_id: &str,
    coupon_id: u64,
    input: GrantInput,
) -> Result<GrantResult, CouponError> {
    if !has_permission(actor, "coupon.grant") {
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

/// 发放核心（旧版 GrantInTx）：先检查首次请求重放，再按 用户 → 模板 → 发放行
/// 加锁，复查请求和额度后发券。同请求且用户、模板一致时返回首次发放 ID；
/// 条件不满足或内容不一致返回冲突。audit 上下文给定时与发放同事务写审计
/// （旧版 AppendAuditInTx 在同一事务追加）；活动发放（批 3）复用时不传。
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
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 首次重放检查（无锁读；命中即同请求复用，读事务无写入直接提交）。
    if let Some(receipt) = find_grant_receipt(&mut *c, request_id, false).await? {
        let grant_id = replay_grant_or_conflict(receipt, request_id, coupon_id, user_id)?;
        tx.commit().await?;
        return Ok(grant_id);
    }
    // 用户行锁（锁序第一环）。
    let Some(status) = identity_repo::lock_status_tx(&mut *c, user_id).await? else {
        return Err(CouponError::Conflict("优惠券发放条件不满足".into()));
    };
    // 模板锁 + 发放计数（模板锁之后不能反向等待用户或钱包）。
    let Some(coupon) = load_coupon(&mut *c, coupon_id, true).await? else {
        return Err(CouponError::NotFound);
    };
    let (total_granted, user_granted): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), \
         COALESCE(CAST(SUM(`user_id` = ?) AS SIGNED), 0) \
         FROM `coupon_grant` WHERE `coupon_id` = ? FOR UPDATE",
    )
    .bind(user_id as i64)
    .bind(coupon_id as i64)
    .fetch_one(&mut *c)
    .await?;
    // 锁后二次重放检查（旧版 findGrantReceipt current 读，FOR SHARE 防并发写入）。
    if let Some(receipt) = find_grant_receipt(&mut *c, request_id, true).await? {
        let grant_id = replay_grant_or_conflict(receipt, request_id, coupon_id, user_id)?;
        tx.commit().await?;
        return Ok(grant_id);
    }
    // 准入：用户 active + 模板启用/时间有效/额度未满（旧版 CheckGrantable）。
    let template = chargepilot_domain::marketing::CouponTemplate {
        template_id: coupon.id,
        // 行读取经 DB 词表严格解码，视图 String 此处回 parse（非法即输入错误）。
        status: chargepilot_domain::marketing::CouponStatus::parse(&coupon.status)
            .map_err(|_| CouponError::Input("优惠券状态无效".into()))?,
        total_quota: coupon.total_quota,
        per_user_quota: coupon.per_user_quota,
        valid_hours: coupon.valid_hours,
        start_at: coupon.start_at,
        end_at: coupon.end_at,
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
    // 发放记录（source_event_id 唯一键兜底并发重放）。
    let result = sqlx::query(
        "INSERT INTO `coupon_grant` (`coupon_id`, `user_id`, `grant_source`, `status`, \
         `expired_at`, `source_event_id`) VALUES (?, ?, 'manual', 'unused', ?, ?)",
    )
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .bind(expires_at)
    .bind(request_id)
    .execute(&mut *c)
    .await?;
    let grant_id = result.last_insert_id();
    sqlx::query(
        "INSERT INTO `coupon_grant_request` (`request_id`, `coupon_id`, `user_id`, `coupon_grant_id`) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .bind(grant_id as i64)
    .execute(&mut *c)
    .await?;
    // 审计与发放同事务（旧版顺序：先发券后追加审计）。
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

/// 重放判定：同请求同券同人返回首次发放 ID；不一致返回冲突（旧版 ReplayGrant
/// 不一致时返回 ErrCouponGrant，HTTP 层映射为 operations.ErrConflict）。
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

/// 按请求号读取发放回执（旧版 findGrantReceipt）；current=true 时取共享锁。
/// request_id 列为 ascii_bin，绑定参数经隐式转换安全比较。
async fn find_grant_receipt(
    c: &mut sqlx::MySqlConnection,
    request_id: &str,
    current: bool,
) -> anyhow::Result<Option<chargepilot_domain::marketing::GrantReceipt>> {
    let sql = format!(
        "SELECT `coupon_id`, `user_id`, `coupon_grant_id` FROM `coupon_grant_request` \
         WHERE `request_id` = ?{}",
        if current { " FOR SHARE" } else { "" }
    );
    let row: Option<(u64, u64, u64)> = sqlx::query_as(&sql)
        .bind(request_id)
        .fetch_optional(c)
        .await?;
    Ok(row.map(
        |(coupon_id, user_id, grant_id)| chargepilot_domain::marketing::GrantReceipt {
            request_id: request_id.to_string(),
            coupon_id,
            user_id,
            coupon_grant_id: grant_id,
        },
    ))
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

// ---------- 报价与核销（旧版 Quote / RedeemInTx + coupon_redeem_repo.go） ----------

/// 券域错误的营销专用变体：HTTP 下单链路按旧版码值映射
/// （NotFound→400、Exhausted→409/2001、Threshold→400），与通用 Conflict 区分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouponRedeemError {
    /// 券不存在、不属于该用户或模板不可用。
    NotFound,
    /// 券已使用、已到期或状态不再为 unused。
    Exhausted,
    /// 原价非正或低于使用门槛。
    Threshold,
    /// 核销参数或冻结优惠与首次不一致。
    Mismatch,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for CouponRedeemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CouponRedeemError::NotFound => f.write_str("优惠券不存在或未发放给该账号"),
            CouponRedeemError::Exhausted => f.write_str("优惠券已过期或已使用"),
            CouponRedeemError::Threshold => f.write_str("未达到该优惠券的使用门槛"),
            CouponRedeemError::Mismatch => f.write_str("优惠券核销信息不一致"),
            CouponRedeemError::Storage(m) => write!(f, "优惠券存储错误: {m}"),
        }
    }
}

impl std::error::Error for CouponRedeemError {}

impl From<sqlx::Error> for CouponRedeemError {
    fn from(e: sqlx::Error) -> Self {
        CouponRedeemError::Storage(e.to_string())
    }
}

/// 发放券组合模板规则（旧版 CouponGrant 组合行）；percent 经 CAST AS CHAR。
#[derive(Debug, Clone)]
pub struct GrantWithRule {
    pub grant_id: u64,
    pub coupon_id: u64,
    pub status: chargepilot_domain::marketing::GrantStatus,
    pub expires_at: DateTime<Utc>,
    pub used_payment_order_id: Option<u64>,
    pub discount_type: String,
    pub discount_value_cents: Option<i64>,
    pub discount_percent: Option<f64>,
    pub free_minutes: u32,
    pub min_charge_cents: i64,
}

type GrantTup = (
    u64,
    u64,
    chargepilot_domain::marketing::GrantStatus,
    DateTime<Utc>,
    Option<u64>,
    String,
    Option<i64>,
    Option<String>,
    i64,
    i64,
);

/// 按用户和发放 ID 读取启用模板的券（旧版 loadGrant）；locked=true 时先锁
/// 模板再锁发放（模板→发放锁序，与活动发放兼容）。
async fn load_grant(
    c: &mut sqlx::MySqlConnection,
    user_id: u64,
    grant_id: u64,
    locked: bool,
) -> Result<Option<GrantWithRule>, CouponRedeemError> {
    if locked {
        // 先按发放行定位不可变模板 ID，再锁模板行（旧版两段锁序）。
        let coupon_id: Option<u64> = sqlx::query_scalar(
            "SELECT `coupon_id` FROM `coupon_grant` \
             WHERE `id` = ? AND `user_id` = ? AND `deleted_at` IS NULL",
        )
        .bind(grant_id as i64)
        .bind(user_id as i64)
        .fetch_optional(&mut *c)
        .await?;
        let Some(coupon_id) = coupon_id else {
            return Ok(None);
        };
        let template: Option<u64> = sqlx::query_scalar(
            "SELECT `id` FROM `coupon` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
        )
        .bind(coupon_id as i64)
        .fetch_optional(&mut *c)
        .await?;
        if template.is_none() {
            return Ok(None);
        }
    }
    let lock_clause = if locked { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT g.`id`, g.`coupon_id`, g.`status`, g.`expired_at`, g.`used_payment_order_id`, \
         c.`discount_type`, c.`discount_value_cents`, CAST(c.`discount_percent` AS CHAR), \
         COALESCE(c.`free_minutes`, 0), c.`min_charge_cents` \
         FROM `coupon_grant` AS g \
         JOIN `coupon` AS c ON c.`id` = g.`coupon_id` AND c.`deleted_at` IS NULL AND c.`status` = 'active' \
         WHERE g.`id` = ? AND g.`user_id` = ? AND g.`deleted_at` IS NULL{lock_clause}"
    );
    let row: Option<GrantTup> = sqlx::query_as(&sql)
        .bind(grant_id as i64)
        .bind(user_id as i64)
        .fetch_optional(&mut *c)
        .await?;
    Ok(row.map(
        |(
            grant_id,
            coupon_id,
            status,
            expires_at,
            used_payment_order_id,
            discount_type,
            discount_value_cents,
            percent_raw,
            free_minutes,
            min_charge_cents,
        )| GrantWithRule {
            grant_id,
            coupon_id,
            status,
            expires_at,
            used_payment_order_id,
            discount_type,
            discount_value_cents,
            discount_percent: percent_raw.and_then(|s| s.parse().ok()),
            // COALESCE(NULL 兜底 0) 把 INT UNSIGNED 提升为 SIGNED BIGINT，
            // i64 解码后收窄（列值 1–1440）。
            free_minutes: free_minutes as u32,
            min_charge_cents,
        },
    ))
}

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

/// 计算本人未用、未到期券对原价的优惠，单位分，不核销券（旧版 Quote）。
pub async fn quote(
    pool: &MySqlPool,
    user_id: u64,
    grant_id: u64,
    total_cents: i64,
) -> Result<i64, CouponRedeemError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some(grant) = load_grant(&mut *c, user_id, grant_id, false).await? else {
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

/// 核销回执（旧版 redemptionReceipt）：重试比对首次结果。
struct RedemptionReceipt {
    coupon_id: u64,
    reference: String,
    discount_cents: i64,
}

/// 在券锁内读取支付对应的充电核销记录（旧版 findRedemption；当前读 FOR UPDATE）。
async fn find_redemption(
    c: &mut sqlx::MySqlConnection,
    payment_order_id: u64,
) -> Result<Option<RedemptionReceipt>, CouponRedeemError> {
    let row: Option<(Option<u64>, Option<String>, i64)> = sqlx::query_as(
        "SELECT `coupon_id`, CAST(`order_no` AS CHAR), `discount_cents` FROM `coupon_redemption` \
         WHERE `biz_type` = 'charge' AND `biz_id` = ? FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    Ok(
        row.map(|(coupon_id, order_no, discount_cents)| RedemptionReceipt {
            coupon_id: coupon_id.unwrap_or(0),
            reference: order_no.unwrap_or_default(),
            discount_cents,
        }),
    )
}

/// 在资金事务内锁定并核销券（旧版 RedeemInTx）：同一券和支付记录重放沿用
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
    let Some(grant) = load_grant(c, user_id, grant_id, true).await? else {
        return Err(CouponRedeemError::NotFound);
    };
    // 已用：仅同一支付的同额核销视为重放成功。
    if grant.status == GrantStatus::Used {
        if grant.used_payment_order_id != Some(payment_order_id) {
            return Err(CouponRedeemError::Exhausted);
        }
        let Some(receipt) = find_redemption(c, payment_order_id).await? else {
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
    // 条件更新到 used：并发竞争失败按已用处理（旧版 RowsAffected == 1）。
    let updated = sqlx::query(
        "UPDATE `coupon_grant` SET `status` = ?, `used_payment_order_id` = ?, `used_at` = ? \
         WHERE `id` = ? AND `user_id` = ? AND `status` = ?",
    )
    .bind(GrantStatus::Used)
    .bind(payment_order_id as i64)
    .bind(Utc::now())
    .bind(grant_id as i64)
    .bind(user_id as i64)
    .bind(GrantStatus::Unused)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(CouponRedeemError::Exhausted);
    }
    sqlx::query(
        "INSERT INTO `coupon_redemption` (`coupon_id`, `user_id`, `biz_type`, `biz_id`, \
         `discount_cents`, `order_no`) VALUES (?, ?, 'charge', ?, ?, ?)",
    )
    .bind(grant.coupon_id as i64)
    .bind(user_id as i64)
    .bind(payment_order_id as i64)
    .bind(discount_cents)
    .bind(reference)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 充电行尚不存在时的稳定核销单号（旧版 pendingChargeNoFor）。
pub fn pending_charge_no_for(payment_order_id: u64) -> String {
    format!("PENDING-{payment_order_id}")
}
