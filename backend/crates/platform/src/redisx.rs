//! Redis 连接池（deadpool）。用途：会话/MFA/频次/租锁；事件投递不再使用 Redis（ADR 002）。

use deadpool_redis::{Config as PoolConfig, Runtime};

pub type Pool = deadpool_redis::Pool;

/// 从 URL 构建连接池。
pub fn open_pool(redis_url: &str, max_size: usize) -> anyhow::Result<Pool> {
    let cfg = PoolConfig::from_url(redis_url);
    let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
    let _ = pool.resize(max_size);
    Ok(pool)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn ping_real_redis() {
        let Ok(url) = std::env::var("TEST_REDIS_URL") else {
            return;
        };
        if url.is_empty() {
            return;
        }
        let pool = super::open_pool(&url, 2).unwrap();
        let mut conn = pool.get().await.unwrap();
        let pong: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();
        assert_eq!(pong, "PONG");
    }
}
