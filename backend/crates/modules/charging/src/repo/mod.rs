//! charging 域表访问（蓝图第五章 repo 层）：SQL 只出现在本目录。
//! 回执、截止、命令与退款登记的读写原语按用例分文件，
//! 本文件放跨用例共用原语（启动失败路径在 receipts 与 sweeper 同构）。

pub mod dispatch;
pub mod receipts;
pub mod stop;
pub mod sweeper;

use sqlx::MySqlConnection;

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}

/// 登记自动渠道退款（启动被拒与超时兜底同款 INSERT SELECT；
/// 退款执行成功后由执行器收尾 refunding → refunded）。
pub async fn insert_charge_refund_tx(
    c: &mut MySqlConnection,
    refund_no: &str,
    payment_order_id: u64,
    order_id: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
         SELECT ?, ?, c.`user_id`, 'charge', c.`id`, p.`paid_cents`, ?, ?, ?, c.`created_month` \
         FROM `charge_order` c JOIN `payment_order` p ON p.`id` = ? \
         WHERE c.`id` = ?",
    )
    .bind(refund_no)
    .bind(payment_order_id as i64)
    .bind(reason)
    .bind(chargepilot_domain::money::RefundStatus::Pending)
    .bind(chargepilot_domain::money::ExecutionPolicy::Automatic)
    .bind(payment_order_id as i64)
    .bind(order_id as i64)
    .execute(c)
    .await?;
    Ok(())
}

/// 读取支付意图上的支付单号（启动失败与超时退款共用）。
pub async fn intent_payment_id_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<u64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `payment_order_id` FROM `charge_payment_intent` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(c)
    .await
}

/// 释放端口预占后把支付意图置为待退款（Paid → RefundRequired）。
pub async fn mark_intent_refund_required_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ? \
         WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(chargepilot_domain::payment::PaymentIntentStatus::RefundRequired)
    .bind(order_id as i64)
    .bind(chargepilot_domain::payment::PaymentIntentStatus::Paid)
    .execute(c)
    .await?;
    Ok(())
}
