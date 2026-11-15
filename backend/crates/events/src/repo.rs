//! `domain_event` 表访问（events 域独占写，见 docs/design/schema.md）。

use crate::EnqueuedEvent;
use sqlx::MySqlPool;

/// 领取一批到期事件（单实例假设下直接领取；处理器幂等兜底重复投递，ADR 002/005）。
pub async fn claim_batch(pool: &MySqlPool, limit: u32) -> anyhow::Result<Vec<EnqueuedEvent>> {
    let rows = sqlx::query_as(
        "SELECT `id`, `event_id`, `source`, `kind`, `payload_json`, `retry_count` \
         FROM `domain_event` \
         WHERE `status` IN (?, ?) AND `scheduled_at` <= NOW(3) \
         ORDER BY `id` LIMIT ?",
    )
    .bind(crate::EventDeliveryStatus::Pending)
    .bind(crate::EventDeliveryStatus::Failed)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, event_id, source, kind, payload, retry_count): (
                u64,
                String,
                String,
                String,
                serde_json::Value,
                u64,
            )| EnqueuedEvent {
                id,
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
    sqlx::query("UPDATE `domain_event` SET `status`=?, `published_at`=NOW(3), `last_error`=NULL WHERE `id`=?")
        .bind(crate::EventDeliveryStatus::Published)
        .bind(id as i64)
        .execute(pool)
        .await?;
    Ok(())
}

/// 投递重试上限：第 10 次失败置死信（退避累计约 17 分钟；DB 列无约束，
/// dead 为词表扩展值，零迁移）。
const MAX_RETRY: u32 = 10;

/// 投递失败：failed + 指数退避重排（`1 << min(retry,8)` 秒）；
/// 超上限置 dead 死信终态（调用方告警日志，人工排查后处理）。
pub async fn mark_failed(
    pool: &MySqlPool,
    id: u64,
    retry_count: u32,
    err: &str,
) -> anyhow::Result<()> {
    let next_retry = retry_count + 1;
    if next_retry >= MAX_RETRY {
        tracing::error!(event_id = id, retry = next_retry, error = %err, "事件投递重试超上限，置死信");
        sqlx::query(
            "UPDATE `domain_event` SET `status`=?, `retry_count`=?, `last_error`=? WHERE `id`=?",
        )
        .bind(crate::EventDeliveryStatus::Dead)
        .bind(next_retry as i64)
        .bind(truncate(err, 250))
        .bind(id as i64)
        .execute(pool)
        .await?;
        return Ok(());
    }
    let backoff_secs: i64 = 1i64 << retry_count.min(8);
    sqlx::query(
        "UPDATE `domain_event` SET `status`=?, `retry_count`=?, `last_error`=?, \
         `scheduled_at`=DATE_ADD(NOW(3), INTERVAL ? SECOND) WHERE `id`=?",
    )
    .bind(crate::EventDeliveryStatus::Failed)
    .bind(next_retry as i64)
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
