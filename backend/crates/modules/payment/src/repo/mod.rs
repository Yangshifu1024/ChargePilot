//! payment 域表访问（蓝图第五章 repo 层）：SQL 只出现在本目录。
//! 回调幂等表（payment_callback_idempotent）读写为 apply 两个业务分支共用，
//! 上移本文件；其余按用例分文件。

pub mod apply;
pub mod card_funding;
pub mod checkout;
pub mod recharge_bridge;
pub mod scan;
pub mod simulation;
pub mod sources;

use sqlx::{MySqlConnection, MySqlPool};

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}

/// 回调幂等摘要读取（同渠道交易号必须同摘要，红线：不同报文冲突）。
pub async fn find_callback_digest(
    conn: &mut MySqlConnection,
    transaction_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT `request_digest` FROM `payment_callback_idempotent` \
         WHERE `wechat_transaction_id` = ?",
    )
    .bind(transaction_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(digest,)| digest))
}

/// 登记回调幂等摘要（uk_transaction_id 幂等）。
pub async fn insert_callback_digest_tx(
    conn: &mut MySqlConnection,
    transaction_id: &str,
    digest: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `payment_callback_idempotent` (`wechat_transaction_id`, `request_digest`) VALUES (?, ?)",
    )
    .bind(transaction_id)
    .bind(digest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 预支付参数读取（charge_prepay 以 payment_order_id 唯一）。
pub async fn find_prepay_params(
    pool: &MySqlPool,
    payment_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`params_json` AS CHAR) FROM `charge_prepay` WHERE `payment_order_id` = ?",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(pool)
    .await
}

/// 预支付参数保存（并发保存以先到者为准，rows_affected 交调用方判定）。
pub async fn insert_ignore_prepay_tx(
    pool: &MySqlPool,
    payment_order_id: u64,
    params_json: &str,
    prepay_id: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "INSERT IGNORE INTO `charge_prepay` (`payment_order_id`, `params_json`, `prepay_id`) \
         VALUES (?, CAST(? AS JSON), ?)",
    )
    .bind(payment_order_id as i64)
    .bind(params_json)
    .bind(prepay_id)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}
