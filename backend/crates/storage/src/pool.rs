//! 单库连接池（ADR 001：单 database、单连接池）。

use sqlx::MySqlPool;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use std::time::Duration;

/// 打开 MySQL 连接池（会话时区 UTC、字符集 utf8mb4）。
pub async fn open_pool(database_url: &str, max_connections: u32) -> anyhow::Result<MySqlPool> {
    let opts: MySqlConnectOptions = database_url
        .parse::<MySqlConnectOptions>()?
        .charset("utf8mb4")
        .collation("utf8mb4_unicode_ci")
        .timezone("+00:00".to_string());
    let pool = MySqlPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await?;
    Ok(pool)
}
