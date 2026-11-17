//! settlement 域表访问（蓝图第五章 repo 层）：SQL 只出现在本目录。
//! recorder 是 wallet::RefundRecorder trait 的唯一生产实现；
//! 其余按用例分文件，快照/计量读取在 billing 与 meter_review 间复用。

pub mod bill;
pub mod billing;
pub mod invoice_admin;
pub mod meter_review;
pub mod recorder;
pub mod refund_admin;
pub mod refund_executor;
pub mod user_invoice;

use sqlx::MySqlConnection;

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}
