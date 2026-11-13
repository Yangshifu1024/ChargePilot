//! `domain_event` 表访问（events 域独占写，见 docs/design/schema.md）。

use crate::EnqueuedEvent;
use sqlx::MySqlPool;

/// 领取一批到期事件（单实例假设下直接领取；处理器幂等兜底重复投递，ADR 002/005）。
pub async fn claim_batch(pool: &MySqlPool, limit: u32) -> anyhow::Result<Vec<EnqueuedEvent>> {
    let rows = sqlx::query_as(
        "SELECT `id`, `event_id`, `source`, `kind`, `payload_json`, `retry_count` \
         FROM `domain_event` \
         WHERE `status` IN ('pending','failed') AND `scheduled_at` <= NOW(3) \
         ORDER BY `id` LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, event_id, source, kind, payload, retry_count): (
                i64,
                String,
                String,
                String,
                serde_json::Value,
                i64,
            )| EnqueuedEvent {
                id: id as u64,
                event_id,
                source: source.parse().unwrap_or(crate::EventSource::User),
                kind,
                payload,
                retry_count: retry_count as u32,
            },
        )
        .collect())
}

impl std::str::FromStr for crate::EventSource {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(crate::EventSource::User),
            "admin" => Ok(crate::EventSource::Admin),
            "gateway" => Ok(crate::EventSource::Gateway),
            _ => Err(anyhow::anyhow!("未知事件来源 {s}")),
        }
    }
}

/// 投递成功：置 published。
pub async fn mark_published(pool: &MySqlPool, id: u64) -> anyhow::Result<()> {
    sqlx::query("UPDATE `domain_event` SET `status`='published', `published_at`=NOW(3), `last_error`=NULL WHERE `id`=?")
        .bind(id as i64)
        .execute(pool)
        .await?;
    Ok(())
}

/// 投递失败：failed + 指数退避重排（`1 << min(retry,8)` 秒）。
pub async fn mark_failed(
    pool: &MySqlPool,
    id: u64,
    retry_count: u32,
    err: &str,
) -> anyhow::Result<()> {
    let backoff_secs: i64 = 1i64 << retry_count.min(8);
    sqlx::query(
        "UPDATE `domain_event` SET `status`='failed', `retry_count`=?, `last_error`=?, \
         `scheduled_at`=DATE_ADD(NOW(3), INTERVAL ? SECOND) WHERE `id`=?",
    )
    .bind(retry_count as i64 + 1)
    .bind(truncate(err, 250))
    .bind(backoff_secs)
    .bind(id as i64)
    .execute(pool)
    .await?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
