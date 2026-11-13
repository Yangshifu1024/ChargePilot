//! 兜底扫描统一框架（阶段 08）：各域扫描器注册 → 统一节拍驱动 →
//! 内存态快照（admin 诊断接口）与滞后指标。
//!
//! 语义沿旧版 worker runtime 的 poll：首轮等一个间隔，取消后停止 tick、
//! 等待已开始的回调返回。扫描器必须幂等（ADR 002 红线）。

use crate::lifecycle::CancelToken;
use sqlx::MySqlPool;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// 单个扫描器的一次执行：领取并处理到期事项，返回处理条数。
pub type SweepFuture = Pin<Box<dyn Future<Output = anyhow::Result<usize>> + Send>>;
pub type SweepFn = Arc<dyn Fn(MySqlPool) -> SweepFuture + Send + Sync>;

/// 扫描器运行态（诊断接口输出；跨任务无锁读共享）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepStatus {
    /// 扫描器名（注册名即任务名）。
    pub name: String,
    /// 节拍间隔（秒）。
    pub interval_secs: u64,
    /// 最近一次完成时刻（RFC3339；从未运行为 null）。
    pub last_run_at: Option<String>,
    /// 最近一次耗时（毫秒）。
    pub last_duration_ms: u64,
    /// 最近一次结果摘要。
    pub last_result: String,
    /// 最近一次错误（成功为 null）。
    pub last_error: Option<String>,
    /// 累计运行次数。
    pub run_count: u64,
}

static STATUS: OnceLock<RwLock<BTreeMap<String, SweepStatus>>> = OnceLock::new();

fn status_map() -> &'static RwLock<BTreeMap<String, SweepStatus>> {
    STATUS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 全部扫描器快照（诊断接口读）。
pub fn snapshot() -> Vec<SweepStatus> {
    status_map()
        .read()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default()
}

/// 统一扫描注册表：注册后由 [`SweepRegistry::spawn_all`] 拉起全部循环。
pub struct SweepRegistry {
    pool: MySqlPool,
    specs: Vec<(&'static str, Duration, SweepFn)>,
}

impl SweepRegistry {
    /// 绑定连接池的空注册表。
    #[must_use]
    pub fn new(pool: MySqlPool) -> Self {
        Self {
            pool,
            specs: Vec::new(),
        }
    }

    /// 注册扫描器（同名覆盖上次注册）。
    pub fn register(
        &mut self,
        name: &'static str,
        interval: Duration,
        f: impl Fn(MySqlPool) -> SweepFuture + Send + Sync + 'static,
    ) {
        metrics::describe_gauge!("sweep_lag_seconds", "兜底扫描滞后（秒）");
        self.specs.push((name, interval, Arc::new(f)));
    }

    /// 拉起全部扫描循环（受管理任务；停机时统一取消并等待）。
    pub fn spawn_all(self, tasks: &mut crate::lifecycle::TaskManager, token: CancelToken) {
        for (name, interval, f) in self.specs {
            let pool = self.pool.clone();
            let token = token.clone();
            tasks.spawn(format!("sweep-{name}"), async move {
                run_loop(pool, name, interval, f, token).await
            });
        }
    }
}

/// 单个扫描循环：等待节拍 → 执行 → 记录内存态。单次失败只记日志与状态，
/// 不退出循环（一次存储抖动不能停掉兜底扫描；旧版 Go 由 run 内部记日志）。
async fn run_loop(
    pool: MySqlPool,
    name: &'static str,
    interval: Duration,
    f: SweepFn,
    token: CancelToken,
) -> anyhow::Result<()> {
    let mut run_count: u64 = 0;
    loop {
        token.sleep_or_cancelled(interval).await;
        if token.is_cancelled() {
            return Ok(());
        }
        let started = Instant::now();
        let result = f(pool.clone()).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        run_count += 1;
        let (last_result, last_error) = match &result {
            Ok(n) => {
                // 刚完成一轮：滞后清零（诊断口径：距上次成功运行的秒数）。
                metrics::gauge!("sweep_lag_seconds", "name" => name).set(0.0);
                (format!("处理 {n} 条"), None)
            }
            Err(e) => {
                tracing::warn!(sweeper = name, error = %e, "兜底扫描执行失败");
                ("失败".to_string(), Some(e.to_string()))
            }
        };
        if let Ok(mut map) = status_map().write() {
            map.insert(
                name.to_string(),
                SweepStatus {
                    name: name.to_string(),
                    interval_secs: interval.as_secs(),
                    last_run_at: Some(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    ),
                    last_duration_ms: elapsed_ms,
                    last_result,
                    last_error,
                    run_count,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照按注册名可见且字段齐全（静态表跨测试共享，用唯一名断言）。
    #[tokio::test]
    async fn registry_runs_and_records_status() {
        let pool = sqlx::MySqlPool::connect_lazy("mysql://invalid:0/none").unwrap();
        let mut tasks = crate::lifecycle::TaskManager::new();
        let token = tasks.token();
        let mut reg = SweepRegistry::new(pool.clone());
        let name = "probe-status-registry_runs";
        reg.register(name, Duration::from_millis(30), move |_pool| {
            Box::pin(async { Ok(7_usize) }) as SweepFuture
        });
        reg.spawn_all(&mut tasks, token);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let snap = snapshot();
        let mine = snap.iter().find(|s| s.name == name).expect("状态已记录");
        assert_eq!(mine.interval_secs, 0);
        assert!(mine.last_run_at.is_some(), "已运行过");
        assert_eq!(mine.last_result, "处理 7 条");
        assert!(mine.last_error.is_none());
        assert!(mine.run_count >= 1);
    }

    /// 扫描器失败不退出循环：错误进内存态，计数继续增长。
    #[tokio::test]
    async fn failing_sweeper_keeps_running() {
        let pool = sqlx::MySqlPool::connect_lazy("mysql://invalid:0/none").unwrap();
        let mut tasks = crate::lifecycle::TaskManager::new();
        let token = tasks.token();
        let mut reg = SweepRegistry::new(pool);
        let name = "probe-fail-failing_sweeper";
        reg.register(name, Duration::from_millis(30), move |_pool| {
            Box::pin(async { Err(anyhow::anyhow!("存储抖动")) }) as SweepFuture
        });
        reg.spawn_all(&mut tasks, token);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let snap = snapshot();
        let mine = snap.iter().find(|s| s.name == name).expect("状态已记录");
        assert!(mine.last_error.is_some(), "错误已记录");
        assert!(mine.run_count >= 2, "失败后循环未退出");
    }
}
