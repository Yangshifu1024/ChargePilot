//! 号段模式 ID 分配器（ADR 003）：内存号段 + 水位 CAS 持久化 + 资金强一致选项。
//!
//! 复用 `snowflake_state` 表（id=1 行）：`last_millisecond` 列语义从旧版「毫秒时间戳」
//! 变更为「已分配上界水位」（表结构不变；旧版雪花实现废弃，列 COMMENT 的历史语义
//! 在下一次 DDL 修订时更新）。水位 CAS：`UPDATE ... WHERE last_millisecond = 旧值`，
//! 号段耗尽且水位未落库时阻塞等待，不静默重用。

use sqlx::MySqlPool;
use std::sync::Mutex;

const WATERMARK_ROW_ID: i64 = 1;

#[derive(Debug)]
struct Segment {
    next: u64,
    max: u64, // 含义：当前号段上界（可分配的最大值）
}

/// 号段分配器。clone 共享同一实例（内部 Mutex）。
#[derive(Clone)]
pub struct SegmentAllocator {
    pool: MySqlPool,
    segment_size: i64,
    seg: std::sync::Arc<Mutex<Option<Segment>>>,
}

impl SegmentAllocator {
    pub fn new(pool: MySqlPool, segment_size: i64) -> Self {
        Self {
            pool,
            segment_size,
            seg: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// 从 DB 读取当前水位并预取首个号段（启动时调用，校验水位行存在）。
    /// CAS 失败（并发写者已推进水位）时重读重试，保证不与任何实例号段重叠。
    pub async fn warm_up(&self) -> anyhow::Result<()> {
        loop {
            let watermark: u64 = sqlx::query_scalar(
                "SELECT `last_millisecond` FROM `snowflake_state` WHERE `id` = ?",
            )
            .bind(WATERMARK_ROW_ID)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("snowflake_state 水位行缺失"))?;
            let new_max = watermark + self.segment_size as u64;
            let rows = self.cas_watermark(watermark, new_max).await?;
            if rows == 0 {
                continue; // 被并发写者抢先：重读重试
            }
            *self.seg.lock().unwrap_or_else(|e| e.into_inner()) = Some(Segment {
                next: watermark + 1,
                max: new_max,
            });
            return Ok(());
        }
    }

    /// 进程内发号：纯内存操作；号段耗尽时异步补号段（阻塞等待，不静默重用）。
    pub async fn next_id(&self) -> anyhow::Result<u64> {
        loop {
            {
                let mut guard = self.seg.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(seg) = guard.as_mut()
                    && seg.next <= seg.max
                {
                    let id = seg.next;
                    seg.next += 1;
                    return Ok(id);
                }
            }
            // 号段耗尽：补号段（锁外执行 IO）
            self.refill().await?;
        }
    }

    /// 补号段：CAS 推进水位。CAS 冲突（并发写者）自动重读重试。
    async fn refill(&self) -> anyhow::Result<()> {
        let watermark: u64 =
            sqlx::query_scalar("SELECT `last_millisecond` FROM `snowflake_state` WHERE `id` = ?")
                .bind(WATERMARK_ROW_ID)
                .fetch_one(&self.pool)
                .await?;
        let new_max = watermark + self.segment_size as u64;
        let rows = self.cas_watermark(watermark, new_max).await?;
        if rows == 0 {
            // 有并发写者已推进水位：重读重试（正常仅单实例，此为防御路径）
            return Ok(());
        }
        let mut guard = self.seg.lock().unwrap_or_else(|e| e.into_inner());
        let start = guard.as_ref().map(|s| s.next).unwrap_or(watermark + 1);
        let new_next = start.max(watermark + 1);
        *guard = Some(Segment {
            next: new_next,
            max: new_max.max(new_next - 1),
        });
        Ok(())
    }

    async fn cas_watermark(&self, old: u64, new: u64) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE `snowflake_state` SET `last_millisecond` = ? WHERE `id` = ? AND `last_millisecond` = ?",
        )
        .bind(new)
        .bind(WATERMARK_ROW_ID)
        .bind(old)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 资金强一致路径：在业务事务内取下一个号段并推进水位（同事务提交，崩溃回滚则号段未发）。
    /// 调用方把返回的 tx 交给后续资金写入，保证「ID 与记录同事务」。
    pub async fn next_id_in_tx(&self, tx: &mut sqlx::MySqlConnection) -> anyhow::Result<u64> {
        let watermark: u64 = sqlx::query_scalar(
            "SELECT `last_millisecond` FROM `snowflake_state` WHERE `id` = ? FOR UPDATE",
        )
        .bind(WATERMARK_ROW_ID)
        .fetch_one(&mut *tx)
        .await?;
        let new_max = watermark + self.segment_size as u64;
        sqlx::query("UPDATE `snowflake_state` SET `last_millisecond` = ? WHERE `id` = ?")
            .bind(new_max)
            .bind(WATERMARK_ROW_ID)
            .execute(&mut *tx)
            .await?;
        Ok((watermark + 1) as u64)
    }
}

/// UUID v4 生成（rand 驱动；命令 ID、意图 ID 与幂等请求共用格式）。
pub fn uuid_v4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 号段测试共享 snowflake_state 水位行，必须串行执行（进程级互斥）；
    /// 用 tokio 异步锁：测试体持锁跨多个 await，std 锁会触发 await_holding_lock。
    fn id_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
    fn test_pool_url() -> String {
        std::env::var("TEST_MYSQL_URL").unwrap_or_default()
    }

    /// 门控语义：未设 TEST_MYSQL_URL 属调用方式错误（应经 test-integration 入口），
    /// 连接失败属环境故障——两者都必须判失败，不允许静默跳过制造假绿。
    async fn fresh_pool() -> MySqlPool {
        let url = test_pool_url();
        if url.is_empty() {
            panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
        }
        // 共享水位绝不回拨：库中已有历史发号，重置会令后续测试
        // 从旧区间重新发号，撞出主键冲突（1062）。号段本应只进不退。
        MySqlPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("数据库连接失败：{e}"))
    }

    #[ignore = "真库集成：node tasks.mjs test-integration"]
    #[tokio::test]
    async fn concurrent_ids_unique_and_monotonic() {
        let _guard = id_test_lock().lock().await;
        let pool = fresh_pool().await;
        let alloc = SegmentAllocator::new(pool.clone(), 100);
        alloc.warm_up().await.unwrap();
        let alloc = std::sync::Arc::new(alloc);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let a = alloc.clone();
            handles.push(tokio::spawn(async move {
                let mut got = Vec::new();
                for _ in 0..50 {
                    got.push(a.next_id().await.unwrap());
                }
                got
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.unwrap());
        }
        all.sort();
        let uniq = all.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(all.len(), 400);
        assert_eq!(uniq.len(), 400, "并发取号必须全库唯一");
        pool.close().await;
    }

    #[ignore = "真库集成：node tasks.mjs test-integration"]
    #[tokio::test]
    async fn watermark_survives_restart() {
        let _guard = id_test_lock().lock().await;
        let pool = fresh_pool().await;
        let a1 = SegmentAllocator::new(pool.clone(), 10);
        a1.warm_up().await.unwrap();
        let first = a1.next_id().await.unwrap();
        drop(a1);
        // 新实例（模拟重启）：不得复用已分配区间
        let a2 = SegmentAllocator::new(pool.clone(), 10);
        a2.warm_up().await.unwrap();
        let second = a2.next_id().await.unwrap();
        assert!(
            second > first,
            "重启后不得回拨: first={first} second={second}"
        );
        pool.close().await;
    }
}
