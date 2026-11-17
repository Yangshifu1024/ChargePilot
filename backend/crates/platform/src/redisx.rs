//! Redis 连接池（deadpool）。用途：会话/MFA/频次/租锁；事件投递不再使用 Redis（ADR 002）。
//! 连通性由集成测试轮验证（会话/频次/租锁等路径真实收发 Redis 命令），
//! 单元测试不对真库做 ping 探活。

use deadpool_redis::{Config as PoolConfig, Runtime};

pub type Pool = deadpool_redis::Pool;

/// 从 URL 构建连接池。
pub fn open_pool(redis_url: &str, max_size: usize) -> anyhow::Result<Pool> {
    let cfg = PoolConfig::from_url(redis_url);
    let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
    pool.resize(max_size);
    Ok(pool)
}
