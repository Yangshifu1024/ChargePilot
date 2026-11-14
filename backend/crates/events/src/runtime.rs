//! 事件投递运行时：扫描到期事件 → 按 kind 路由到处理器 → 标记 published / 退避重排。
//!
//! 作为受管理任务由 TaskManager 驱动（platform::lifecycle）；停机时通过 CancelToken 退出。

use crate::{EnqueuedEvent, EventDispatcher, EventKind, repo};
use chargepilot_platform::lifecycle::CancelToken;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Arc;

/// kind → 处理器登记表。处理器必须幂等（ADR 002 红线）。
/// 键为 [`EventKind`] 进程内词表：未知 kind（历史行/未来类别）在路由处
/// 归入「无已登记处理器」路径，与旧行为一致。
#[derive(Default)]
pub struct Registry {
    handlers: HashMap<EventKind, Arc<dyn EventDispatcher>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: EventKind, handler: Arc<dyn EventDispatcher>) {
        self.handlers.insert(kind, handler);
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn EventDispatcher>> {
        EventKind::parse(kind).and_then(|k| self.handlers.get(&k).cloned())
    }
}

/// 投递循环。每轮：领取批次 → 逐条投递 → 标记。节拍 1s（有事件时连续处理）。
pub async fn spawn_dispatch_loop(
    pool: MySqlPool,
    registry: Arc<Registry>,
    token: CancelToken,
) -> anyhow::Result<()> {
    loop {
        if token.is_cancelled() {
            return Ok(());
        }
        let batch = repo::claim_batch(&pool, 100).await?;
        if batch.is_empty() {
            token
                .sleep_or_cancelled(std::time::Duration::from_secs(1))
                .await;
            continue;
        }
        for event in &batch {
            dispatch_one(&pool, &registry, event).await;
        }
    }
}

async fn dispatch_one(pool: &MySqlPool, registry: &Registry, event: &EnqueuedEvent) {
    let started = std::time::Instant::now();
    let result = match registry.get(&event.kind) {
        None => Err(anyhow::anyhow!("kind '{}' 无已登记处理器", event.kind)),
        Some(handler) => handler.dispatch(event).await,
    };
    metrics::histogram!("event_dispatch_latency_seconds").record(started.elapsed().as_secs_f64());
    match result {
        Ok(()) => {
            if let Err(e) = repo::mark_published(pool, event.id).await {
                tracing::error!(event_id = %event.event_id, error = %e, "标记 published 失败");
            } else {
                tracing::info!(event_id = %event.event_id, kind = %event.kind, "事件投递完成");
            }
        }
        Err(e) => {
            metrics::counter!("event_dispatch_failed_total").increment(1);
            if let Err(mark_err) =
                repo::mark_failed(pool, event.id, event.retry_count, &e.to_string()).await
            {
                tracing::error!(event_id = %event.event_id, error = %mark_err, "标记 failed 失败");
            }
        }
    }
}
