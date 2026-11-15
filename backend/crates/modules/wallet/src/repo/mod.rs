//! wallet 域表访问与读模型（蓝图第五章 repo 层）：SQL 只出现在本目录。

pub mod account;
pub mod credit;
pub mod ledger;
pub mod query;
pub mod recharge;
pub mod recharge_history;
pub mod refund;
pub mod risk;

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置，充值/退款/风控共用）。
pub async fn set_read_committed(c: &mut sqlx::MySqlConnection) -> Result<(), crate::WalletError> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}
