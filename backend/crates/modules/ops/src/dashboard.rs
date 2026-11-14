//! 运营仪表盘：多只读查询的日界投影（北京时间日界，多查询不共享事务快照——旧版语义）。

use serde::Serialize;
use sqlx::MySqlPool;

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub charging_orders: i64,
    pub today_orders: i64,
    pub total_users: i64,
    pub new_users: i64,
    pub today_charging_users: i64,
    pub station_count: i64,
    pub device_count: i64,
    pub today_order_users: i64,
    pub today_completed_orders: i64,
    pub today_settled_cents: i64,
    pub active_alerts: i64,
    pub daily_trend: Vec<TrendPoint>,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct TrendPoint {
    pub day: String,
    pub completed_orders: i64,
    pub settled_cents: i64,
}

/// 汇总仪表盘。任一查询失败整体失败（503 由上层映射）。
pub async fn load(pool: &MySqlPool) -> anyhow::Result<Dashboard> {
    // 北京时间自然日 0 点（固定 +8 偏移，旧版语义）
    let now_bj = chrono::Utc::now() + chrono::Duration::hours(8);
    let today_bj = now_bj.date_naive();
    let today_start_utc = match today_bj.and_hms_opt(0, 0, 0) {
        Some(t) => t,
        None => panic!("日期零点恒存在"),
    } - chrono::Duration::hours(8);
    let tomorrow_start_utc = today_start_utc + chrono::Duration::days(1);
    let trend_start_utc = today_start_utc - chrono::Duration::days(6);

    let charging_orders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_order` WHERE `status` = 'charging' AND `deleted_at` IS NULL",
    )
    .fetch_one(pool)
    .await?;
    let today_orders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_order` WHERE `created_at` >= ? AND `created_at` < ? AND `deleted_at` IS NULL",
    )
    .bind(today_start_utc)
    .bind(tomorrow_start_utc)
    .fetch_one(pool)
    .await?;
    let total_users: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `user` WHERE `deleted_at` IS NULL")
            .fetch_one(pool)
            .await?;
    let new_users: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `user` WHERE `created_at` >= ? AND `created_at` < ? AND `deleted_at` IS NULL",
    )
    .bind(today_start_utc)
    .bind(tomorrow_start_utc)
    .fetch_one(pool)
    .await?;
    let today_charging_users: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT `user_id`) FROM `charge_order` WHERE `started_at` >= ? AND `started_at` < ? AND `deleted_at` IS NULL",
    )
    .bind(today_start_utc)
    .bind(tomorrow_start_utc)
    .fetch_one(pool)
    .await?;
    let station_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `station` WHERE `deleted_at` IS NULL")
            .fetch_one(pool)
            .await?;
    let device_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `device_meta` WHERE `deleted_at` IS NULL")
            .fetch_one(pool)
            .await?;
    let today_order_users: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT `user_id`) FROM `charge_order` WHERE `created_at` >= ? AND `created_at` < ? AND `deleted_at` IS NULL",
    )
    .bind(today_start_utc)
    .bind(tomorrow_start_utc)
    .fetch_one(pool)
    .await?;
    let active_alerts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `alert_event` WHERE `status` IN (?, ?)")
            .bind(chargepilot_domain::device::AlertStatus::Active)
            .bind(chargepilot_domain::device::AlertStatus::Acknowledged)
            .fetch_one(pool)
            .await?;

    // 7 日趋势：按东八区日分组，缺数补 0
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT DATE_FORMAT(DATE_ADD(`ended_at`, INTERVAL 8 HOUR), '%Y-%m-%d') AS `day`, \
                COUNT(*) AS `completed_orders`, COALESCE(CAST(SUM(`total_cents`) AS SIGNED), 0) AS `settled_cents` \
         FROM `charge_order` \
         WHERE `ended_at` >= ? AND `ended_at` < ? AND `total_cents` IS NOT NULL AND `deleted_at` IS NULL \
         GROUP BY `day`",
    )
    .bind(trend_start_utc)
    .bind(tomorrow_start_utc)
    .fetch_all(pool)
    .await?;
    let mut daily_trend = Vec::with_capacity(7);
    for offset in (0..7).rev() {
        let day = (today_bj - chrono::Duration::days(offset)).to_string();
        let hit = rows.iter().find(|(d, _, _)| *d == day);
        daily_trend.push(TrendPoint {
            day,
            completed_orders: hit.map(|(_, c, _)| *c).unwrap_or(0),
            settled_cents: hit.map(|(_, _, s)| *s).unwrap_or(0),
        });
    }
    let (today_completed_orders, today_settled_cents) = daily_trend
        .last()
        .map(|p| (p.completed_orders, p.settled_cents))
        .unwrap_or((0, 0));

    Ok(Dashboard {
        charging_orders,
        today_orders,
        total_users,
        new_users,
        today_charging_users,
        station_count,
        device_count,
        today_order_users,
        today_completed_orders,
        today_settled_cents,
        active_alerts,
        daily_trend,
        updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}
