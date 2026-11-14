//! Redis 连接池（deadpool）。用途：会话/MFA/频次/租锁；事件投递不再使用 Redis（ADR 002）。
//! 真实连通性不再单测探活（票 23 删除 ping_real_redis）：真库集成轮的会话/
//! 频次/租锁流全部真实经过本池的取用与命令往返，连接故障即整轮红。

use deadpool_redis::{Config as PoolConfig, Runtime};

pub type Pool = deadpool_redis::Pool;

/// 从 URL 构建连接池。
pub fn open_pool(redis_url: &str, max_size: usize) -> anyhow::Result<Pool> {
    let cfg = PoolConfig::from_url(redis_url);
    let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
    pool.resize(max_size);
    Ok(pool)
}
