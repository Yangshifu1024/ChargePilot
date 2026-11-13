//! 持久化基础：单库连接池与自研迁移器。
//!
//! 迁移器语义（阶段 00 定案）：SQL 基线以 `include_str!` 编译期内嵌，按文件名序执行；
//! 语句按「分号 + 换行」边界切分（已验证 0001_init.sql 全部 236 段引号平衡）；
//! 已应用版本记录在 `schema_migrations` 表，重复执行幂等跳过。

pub mod migrator;
pub mod pool;

pub use pool::open_pool;
