//! coupon / coupon_grant / coupon_grant_request / coupon_redemption 表访问
//! （蓝图第五章 repo 层）：SQL 只出现在本目录。

use crate::error::{CouponError, CouponRedeemError};
use chargepilot_domain::marketing::GrantStatus;
use chrono::{DateTime, Utc};
use sqlx::MySqlConnection;

/// 券模板行（JSON 键名与响应契约一致）。discount_percent 为应付比例百分数
/// （80 表示八折），两位小数；三型优惠列互斥，读取侧不做替换或降级。
#[derive(Debug, Clone, serde::Serialize)]
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

/// 读取未删除模板行；locked=true 时加 FOR UPDATE（发放与更新的锁点）。
pub async fn load_coupon(
    c: &mut MySqlConnection,
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

/// 后台模板列表行（倒序）。
pub async fn coupon_rows_desc(pool: &sqlx::MySqlPool) -> Result<Vec<CouponRow>, CouponError> {
    let rows: Vec<CouponTup> = sqlx::query_as(&format!(
        "SELECT {COUPON_COLUMNS} FROM `coupon` WHERE `deleted_at` IS NULL ORDER BY `id` DESC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(coupon_from).collect())
}

/// 创建启用模板，返回自增主键（状态强制 active；优惠列互斥由调用方校验）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_coupon_tx(
    c: &mut MySqlConnection,
    name: &str,
    discount_type: &str,
    discount_value_cents: Option<i64>,
    discount_percent: Option<f64>,
    free_minutes: Option<u32>,
    min_charge_cents: i64,
    valid_hours: u32,
    total_quota: u32,
    per_user_quota: u32,
    start_at: Option<DateTime<Utc>>,
    end_at: Option<DateTime<Utc>>,
) -> Result<u64, CouponError> {
    let result = sqlx::query(
        "INSERT INTO `coupon` (`name`, `discount_type`, `discount_value_cents`, \
         `discount_percent`, `free_minutes`, `min_charge_cents`, `valid_hours`, \
         `total_quota`, `per_user_quota`, `status`, `start_at`, `end_at`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(name)
    .bind(discount_type)
    .bind(discount_value_cents)
    .bind(discount_percent)
    .bind(free_minutes)
    .bind(min_charge_cents)
    .bind(valid_hours)
    .bind(total_quota)
    .bind(per_user_quota)
    .bind(start_at)
    .bind(end_at)
    .execute(&mut *c)
    .await?;
    Ok(result.last_insert_id())
}

/// 更新模板名称与启停状态（优惠与额度不可修改，由调用方锁定旧值保证）。
pub async fn update_name_status_tx(
    c: &mut MySqlConnection,
    id: u64,
    name: &str,
    status: &str,
) -> Result<(), CouponError> {
    sqlx::query("UPDATE `coupon` SET `name` = ?, `status` = ? WHERE `id` = ?")
        .bind(name)
        .bind(status)
        .bind(id as i64)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 发放统计聚合：(发放总数, 未用, 已用, 已过期)。
/// unused 但已到期计为 expired；SUM 布尔必须 CAST AS SIGNED（布尔聚合返回 DECIMAL）。
pub async fn grant_stats_counts(
    c: &mut MySqlConnection,
    coupon_id: u64,
) -> Result<(i64, i64, i64, i64), CouponError> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*), \
         COALESCE(CAST(SUM(`status`='unused' AND `expired_at`>UTC_TIMESTAMP(3)) AS SIGNED),0), \
         COALESCE(CAST(SUM(`status`='used') AS SIGNED),0), \
         COALESCE(CAST(SUM(`status`='expired' OR (`status`='unused' AND `expired_at`<=UTC_TIMESTAMP(3))) AS SIGNED),0) \
         FROM `coupon_grant` WHERE `coupon_id` = ?",
    )
    .bind(coupon_id as i64)
    .fetch_one(&mut *c)
    .await?)
}

/// 发放行计数（模板锁之后读取；FOR UPDATE）：(总发放, 该用户发放)。
pub async fn grant_counts_tx(
    c: &mut MySqlConnection,
    user_id: u64,
    coupon_id: u64,
) -> Result<(i64, i64), CouponError> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*), \
         COALESCE(CAST(SUM(`user_id` = ?) AS SIGNED), 0) \
         FROM `coupon_grant` WHERE `coupon_id` = ? FOR UPDATE",
    )
    .bind(user_id as i64)
    .bind(coupon_id as i64)
    .fetch_one(&mut *c)
    .await?)
}

/// 按请求号读取发放回执；current=true 时取共享锁。
/// request_id 列为 ascii_bin，绑定参数经隐式转换安全比较。
pub async fn find_grant_receipt(
    c: &mut MySqlConnection,
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

/// 发放记录（source_event_id 唯一键兜底并发重放），返回发放主键。
pub async fn insert_grant_tx(
    c: &mut MySqlConnection,
    coupon_id: u64,
    user_id: u64,
    expires_at: DateTime<Utc>,
    request_id: &str,
) -> Result<u64, CouponError> {
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
    Ok(result.last_insert_id())
}

/// 发放请求回执（幂等重放的比对锚点）。
pub async fn insert_grant_request_tx(
    c: &mut MySqlConnection,
    request_id: &str,
    coupon_id: u64,
    user_id: u64,
    grant_id: u64,
) -> Result<(), CouponError> {
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
    Ok(())
}

/// 发放券组合模板规则（GrantRule 计算输入）；percent 经 CAST AS CHAR。
#[derive(Debug, Clone)]
pub struct GrantWithRule {
    pub grant_id: u64,
    pub coupon_id: u64,
    pub status: GrantStatus,
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
    GrantStatus,
    DateTime<Utc>,
    Option<u64>,
    String,
    Option<i64>,
    Option<String>,
    i64,
    i64,
);

/// 按用户和发放 ID 读取启用模板的券；locked=true 时先锁模板再锁发放
/// （模板→发放锁序，与活动发放兼容）。
pub async fn load_grant(
    c: &mut MySqlConnection,
    user_id: u64,
    grant_id: u64,
    locked: bool,
) -> Result<Option<GrantWithRule>, CouponRedeemError> {
    if locked {
        // 先按发放行定位不可变模板 ID，再锁模板行（两段锁序）。
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

/// 核销回执：重试比对首次结果。
pub struct RedemptionReceipt {
    pub coupon_id: u64,
    pub reference: String,
    pub discount_cents: i64,
}

/// 券锁内读取支付对应的充电核销记录（当前读 FOR UPDATE）。
pub async fn find_redemption(
    c: &mut MySqlConnection,
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

/// 条件更新到 used：并发竞争失败按已用处理；返回推进行数。
pub async fn mark_grant_used_tx(
    c: &mut MySqlConnection,
    grant_id: u64,
    user_id: u64,
    payment_order_id: u64,
) -> Result<u64, CouponRedeemError> {
    Ok(sqlx::query(
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
    .rows_affected())
}

/// 插入核销记录（随调用方资金事务提交）。
pub async fn insert_redemption_tx(
    c: &mut MySqlConnection,
    coupon_id: u64,
    user_id: u64,
    payment_order_id: u64,
    discount_cents: i64,
    reference: &str,
) -> Result<(), CouponRedeemError> {
    sqlx::query(
        "INSERT INTO `coupon_redemption` (`coupon_id`, `user_id`, `biz_type`, `biz_id`, \
         `discount_cents`, `order_no`) VALUES (?, ?, 'charge', ?, ?, ?)",
    )
    .bind(coupon_id as i64)
    .bind(user_id as i64)
    .bind(payment_order_id as i64)
    .bind(discount_cents)
    .bind(reference)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置；调用方随后
/// pool.acquire + conn.begin 开事务——连接级设置随连接保留）。
pub async fn set_read_committed(c: &mut sqlx::MySqlConnection) -> Result<(), CouponError> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}
