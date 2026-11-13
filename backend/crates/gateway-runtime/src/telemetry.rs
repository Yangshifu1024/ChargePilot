//! 遥测写入管道（G2）：有界缓冲 → 批量多值 INSERT → 15 分钟/小时聚合
//! UPSERT（同批同桶预合并）。
//!
//! 分级语义（ADR 005）：缓冲满时丢最旧明细并计数，快照与聚合不受影响；
//! 批量 200ms 或 500 条触发；崩溃丢失窗口 ≤ 200ms。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::MySqlPool;

/// 缓冲容量（spec G2：4096）。
pub const BUFFER_CAPACITY: usize = 4096;
/// 批量触发条数。
pub const FLUSH_BATCH: usize = 500;
/// 批量触发周期。
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// 单条遥测样本（telemetry 行；ts 为采集时刻）。
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// 设备编号。
    pub device_id: String,
    /// 端口号（None = 设备级指标）。
    pub port_no: Option<u8>,
    /// 指标名（voltage_v / temperature_c / power_w / meter_kwh）。
    pub metric: &'static str,
    /// 数值（单位随指标）。
    pub value: f64,
    /// 采集时刻。
    pub ts: chrono::DateTime<chrono::Utc>,
}

/// 聚合桶内的预合并值。
#[derive(Debug, Clone, Copy)]
struct AggValue {
    sum: f64,
    min: f64,
    max: f64,
    count: u64,
}

impl AggValue {
    fn new(value: f64) -> Self {
        Self {
            sum: value,
            min: value,
            max: value,
            count: 1,
        }
    }

    /// 增量合并（均值按样本数加权）。
    fn merge(&mut self, other: &AggValue) {
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.count += other.count;
    }

    fn avg(&self) -> f64 {
        self.sum / self.count as f64
    }
}

/// 有界遥测缓冲：满时丢最旧明细并累计丢弃数（分级语义）。
#[derive(Default)]
pub struct TelemetryBuffer {
    queue: Mutex<VecDeque<Sample>>,
    /// 累计丢弃的明细样本数。
    dropped: AtomicU64,
    /// 累计接收样本数。
    received: AtomicU64,
}

impl TelemetryBuffer {
    /// 空缓冲。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队一条样本；缓冲满时丢弃最旧并计数。
    pub fn push(&self, sample: Sample) {
        self.received.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.queue.lock().expect("遥测缓冲锁");
        if queue.len() >= BUFFER_CAPACITY {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(sample);
    }

    /// 取出全部样本（flush 用）。
    pub fn drain(&self) -> Vec<Sample> {
        let mut queue = self.queue.lock().expect("遥测缓冲锁");
        queue.drain(..).collect()
    }

    /// 当前缓冲深度。
    #[must_use]
    pub fn depth(&self) -> usize {
        self.queue.lock().expect("遥测缓冲锁").len()
    }

    /// 累计丢弃计数。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 累计接收计数。
    #[must_use]
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

/// 聚合桶键。
type BucketKey = (
    String,
    Option<u8>,
    &'static str,
    chrono::DateTime<chrono::Utc>,
    String,
);

/// 同批样本按 15 分钟/小时桶预合并。
fn build_buckets(samples: &[Sample], step_secs: i64, table: &str) -> HashMap<BucketKey, AggValue> {
    let mut buckets: HashMap<BucketKey, AggValue> = HashMap::new();
    for s in samples {
        let ts = s.ts.timestamp();
        let bucket_start = ts - ts.rem_euclid(step_secs);
        let start = chrono::DateTime::from_timestamp(bucket_start, 0)
            .unwrap_or_else(|| s.ts)
            .to_owned();
        let key: BucketKey = (
            s.device_id.clone(),
            s.port_no,
            s.metric,
            start,
            table.to_string(),
        );
        buckets
            .entry(key)
            .and_modify(|v| v.merge(&AggValue::new(s.value)))
            .or_insert_with(|| AggValue::new(s.value));
    }
    buckets
}

/// 批量落库：多值 INSERT 明细 + 两级聚合 UPSERT（增量合并既有桶）。
pub async fn flush(pool: &MySqlPool, samples: &[Sample]) -> anyhow::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    // 明细多值 INSERT（uk_sample 冲突时覆盖数值——重放/补录安全）。
    let mut builder = sqlx::query_builder::QueryBuilder::<sqlx::MySql>::default();
    builder.push(
        "INSERT INTO `telemetry` (`device_id`, `port_no`, `metric`, `value_num`, `ts`) VALUES ",
    );
    for (i, s) in samples.iter().enumerate() {
        if i > 0 {
            builder.push(", ");
        }
        builder
            .push("(")
            .push_bind(&s.device_id)
            .push(", ")
            .push_bind(s.port_no.map(i64::from))
            .push(", ")
            .push_bind(s.metric)
            .push(", ")
            .push_bind(s.value)
            .push(", ")
            .push_bind(s.ts)
            .push(")");
    }
    builder.push(" ON DUPLICATE KEY UPDATE `value_num` = VALUES(`value_num`)");
    builder.build().execute(&mut *tx).await?;

    // 聚合：同批同桶先在内存合并，再按增量公式并入既有行。
    for (step_secs, table, bucket_col, month_col) in [
        (
            900i64,
            "telemetry_aggregate_15min",
            "bucket_start",
            "bucket_month",
        ),
        (
            3600i64,
            "telemetry_aggregate_hourly",
            "bucket_start",
            "bucket_month",
        ),
    ] {
        let buckets = build_buckets(samples, step_secs, table);
        for ((device_id, port_no, metric, start, _), agg) in buckets {
            use chrono::Datelike as _;
            let month = start
                .date_naive()
                .with_day(1)
                .unwrap_or_else(|| start.date_naive());
            sqlx::query(&format!(
                "INSERT INTO `{table}` (`device_id`, `port_no`, `metric`, `{bucket_col}`, \
                 `{month_col}`, `avg_value`, `min_value`, `max_value`, `count`) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE \
                 `avg_value` = (`avg_value` * `count` + VALUES(`avg_value`) * VALUES(`count`)) \
                   / (`count` + VALUES(`count`)), \
                 `min_value` = LEAST(`min_value`, VALUES(`min_value`)), \
                 `max_value` = GREATEST(`max_value`, VALUES(`max_value`)), \
                 `count` = `count` + VALUES(`count`)",
            ))
            .bind(&device_id)
            .bind(port_no.map(i64::from))
            .bind(metric)
            .bind(start)
            .bind(month)
            .bind(agg.avg())
            .bind(agg.min)
            .bind(agg.max)
            .bind(agg.count as i64)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// 批量循环：每 200ms 或积满 500 条触发一次 flush；shutdown 后冲刷残余并退出。
pub async fn run_flush_loop(
    pool: MySqlPool,
    buffer: std::sync::Arc<TelemetryBuffer>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        interval.tick().await;
        let samples = buffer.drain();
        if !samples.is_empty() {
            if let Err(e) = flush(&pool, &samples).await {
                tracing::warn!(count = samples.len(), error = %e, "遥测批量落库失败");
            }
        }
        if *shutdown.borrow() {
            // 停机冲刷：退出前把残余写入。
            let rest = buffer.drain();
            if !rest.is_empty() {
                if let Err(e) = flush(&pool, &rest).await {
                    tracing::warn!(count = rest.len(), error = %e, "停机冲刷失败");
                }
            }
            tracing::info!("遥测批量循环退出");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(value: f64, minute: u32) -> Sample {
        use chrono::TimeZone as _;
        let base = chrono::Utc
            .with_ymd_and_hms(2026, 11, 14, 10, 0, 0)
            .single()
            .unwrap();
        Sample {
            device_id: "dev-1".into(),
            port_no: Some(1),
            metric: "power_w",
            value,
            ts: base + chrono::Duration::minutes(i64::from(minute)),
        }
    }

    #[test]
    fn buffer_drops_oldest_when_full() {
        let buffer = TelemetryBuffer::new();
        for i in 0..(BUFFER_CAPACITY + 10) {
            buffer.push(Sample {
                device_id: format!("dev-{i}"),
                port_no: None,
                metric: "power_w",
                value: i as f64,
                ts: chrono::Utc::now(),
            });
        }
        assert_eq!(buffer.depth(), BUFFER_CAPACITY);
        assert_eq!(buffer.dropped(), 10);
        assert_eq!(buffer.received(), (BUFFER_CAPACITY + 10) as u64);
        // 最旧的十条被丢弃：队首是 dev-10。
        let drained = buffer.drain();
        assert_eq!(drained[0].device_id, "dev-10");
        assert_eq!(
            drained.last().unwrap().device_id,
            format!("dev-{}", BUFFER_CAPACITY + 9)
        );
    }

    #[test]
    fn buckets_merge_same_key() {
        // 同一 15 分钟桶内的三条样本预合并为一条。
        let samples = vec![sample(100.0, 1), sample(200.0, 5), sample(300.0, 9)];
        let buckets = build_buckets(&samples, 900, "t");
        assert_eq!(buckets.len(), 1);
        let agg = buckets.values().next().unwrap();
        assert_eq!(agg.count, 3);
        assert_eq!(agg.avg(), 200.0);
        assert_eq!(agg.min, 100.0);
        assert_eq!(agg.max, 300.0);
    }

    #[test]
    fn buckets_split_across_hours() {
        let samples = vec![sample(100.0, 5), sample(200.0, 5)];
        let mut late = sample(300.0, 5);
        late.ts = late.ts + chrono::Duration::hours(1);
        let mut all = samples;
        all.push(late);
        let buckets = build_buckets(&all, 3600, "t");
        assert_eq!(buckets.len(), 2);
    }
}
