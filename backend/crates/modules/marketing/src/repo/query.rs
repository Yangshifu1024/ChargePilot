//! 用户持券视图：支付前可用券列表与本人券分页。到期状态只用于展示，
//! 不写回数据库；expired 由内存按当前时间派生。

use crate::error::CouponQueryError;
use chargepilot_domain::marketing::GrantStatus;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::MySqlPool;

/// 支付前可用券的展示资料。
#[derive(Debug, Clone, Serialize)]
pub struct AvailableCoupon {
    pub coupon_id: u64,
    pub name: String,
    pub grant_id: u64,
    pub discount_type: String,
    pub min_charge_cents: i64,
    pub expires_at: DateTime<Utc>,
}

/// 本人券列表记录：status 是 coupon_grant.status，
/// 未用但已到期展示 expired（内存派生，不落库）；usable 表示支付时可选。
#[derive(Debug, Clone, Serialize)]
pub struct UserCoupon {
    pub coupon_id: u64,
    pub name: String,
    pub grant_id: u64,
    pub discount_type: String,
    pub discount_value_cents: Option<i64>,
    pub discount_percent: Option<f64>,
    pub min_charge_cents: i64,
    pub status: String,
    pub expired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub usable: bool,
}

/// 分页结果。
#[derive(Debug, Clone, Serialize)]
pub struct CouponPage {
    pub items: Vec<UserCoupon>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    pub count: i64,
}

/// 持券查询投影；percent 经 CAST AS CHAR。
type GrantTup = (
    u64,
    u64,
    String,
    String,
    DateTime<Utc>,
    String,
    Option<i64>,
    Option<String>,
    i64,
);

fn grant_from(
    (id, coupon_id, name, status, expired_at, discount_type, amount, percent_raw, min_charge): GrantTup,
) -> UserCoupon {
    UserCoupon {
        coupon_id,
        name,
        grant_id: id,
        discount_type,
        discount_value_cents: amount,
        discount_percent: percent_raw.and_then(|s| s.parse().ok()),
        min_charge_cents: min_charge,
        status,
        expired_at,
        expires_at: expired_at,
        usable: false,
    }
}

/// 按到期时间返回本人可用券：排除停用、删除、已用和过期；
/// SQL 侧以 UTC_TIMESTAMP(3) 过滤。
pub async fn available(
    pool: &MySqlPool,
    user_id: u64,
) -> Result<Vec<AvailableCoupon>, CouponQueryError> {
    if user_id == 0 {
        return Err(CouponQueryError::Input("用户编号无效".into()));
    }
    let rows: Vec<GrantTup> = sqlx::query_as(
        "SELECT g.`id`, g.`coupon_id`, c.`name`, g.`status`, g.`expired_at`, \
         c.`discount_type`, c.`discount_value_cents`, \
         CAST(c.`discount_percent` AS CHAR) AS `discount_percent`, c.`min_charge_cents` \
         FROM `coupon_grant` AS g \
         JOIN `coupon` AS c ON c.`id` = g.`coupon_id` AND c.`deleted_at` IS NULL AND c.`status` = 'active' \
         WHERE g.`user_id` = ? AND g.`status` = 'unused' AND g.`deleted_at` IS NULL \
           AND g.`expired_at` > UTC_TIMESTAMP(3) \
         ORDER BY g.`expired_at`, g.`id`",
    )
    .bind(user_id as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|t| {
            let u = grant_from(t);
            AvailableCoupon {
                coupon_id: u.coupon_id,
                name: u.name,
                grant_id: u.grant_id,
                discount_type: u.discount_type,
                min_charge_cents: u.min_charge_cents,
                expires_at: u.expires_at,
            }
        })
        .collect())
}

/// 本人券分页：读全部记录后在内存按同一当前时间计算状态、
/// 筛选与分页；排序保持查询序（status → expired_at → id）。
/// `now` 由调用方传入以便测试注入展示时钟。
pub async fn mine(
    pool: &MySqlPool,
    user_id: u64,
    page: i64,
    page_size: i64,
    status: &str,
    only_usable: bool,
    now: DateTime<Utc>,
) -> Result<CouponPage, CouponQueryError> {
    if user_id == 0 || chargepilot_domain::page::PageRequest::user(page, page_size).is_err() {
        return Err(CouponQueryError::Input("分页或用户编号无效".into()));
    }
    if !status.is_empty() && chargepilot_domain::marketing::GrantStatus::parse(status).is_err() {
        return Err(CouponQueryError::Input("优惠券状态无效".into()));
    }
    let rows: Vec<GrantTup> = sqlx::query_as(
        "SELECT g.`id`, g.`coupon_id`, c2.`name`, g.`status`, g.`expired_at`, \
         c2.`discount_type`, c2.`discount_value_cents`, \
         CAST(c2.`discount_percent` AS CHAR) AS `discount_percent`, c2.`min_charge_cents` \
         FROM `coupon_grant` AS g \
         JOIN `coupon` AS c2 ON c2.`id` = g.`coupon_id` AND c2.`deleted_at` IS NULL AND c2.`status` = 'active' \
         WHERE g.`user_id` = ? AND g.`deleted_at` IS NULL \
         ORDER BY g.`status`, g.`expired_at`, g.`id`",
    )
    .bind(user_id as i64)
    .fetch_all(pool)
    .await?;
    let mut items: Vec<UserCoupon> = Vec::new();
    for t in rows {
        let mut item = grant_from(t);
        // 未用但已到期 → 展示 expired。
        let derived = if item.status == GrantStatus::Unused.as_str() && item.expires_at <= now {
            GrantStatus::Expired.as_str().to_owned()
        } else {
            item.status.clone()
        };
        if !status.is_empty() && derived != status {
            continue;
        }
        item.usable = derived == GrantStatus::Unused.as_str() && item.expires_at > now;
        item.status = derived;
        if only_usable && !item.usable {
            continue;
        }
        items.push(item);
    }
    let total = items.len() as i64;
    let start = ((page - 1) * page_size).min(total) as usize;
    let end = (start + page_size as usize).min(items.len());
    Ok(CouponPage {
        items: items[start..end].to_vec(),
        page,
        page_size,
        total,
        count: total,
    })
}
