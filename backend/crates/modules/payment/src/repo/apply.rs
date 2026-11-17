//! 支付成功事务的表访问：payment_order / charge_payment_intent /
//! wallet_recharge_request / refund_record（蓝图第五章 repo 层）。

use chargepilot_domain::money::{BizType, PaymentOrderStatus};
use chargepilot_domain::payment::PaymentIntentStatus;
use sqlx::MySqlConnection;

/// 锁读投影元组（列序即 SELECT 顺序）——type_complexity 要求具名；
/// 词表列直接解出枚举。
pub type PayOrderRow = (
    u64,
    BizType,
    u64,
    u64,
    i64,
    PaymentOrderStatus,
    Option<String>,
    chrono::NaiveDate,
);

/// 先按渠道商户单号锁定支付订单，再按业务类型分流。
pub async fn lock_pay_order_tx(
    conn: &mut MySqlConnection,
    merchant_order_no: &str,
) -> Result<Option<PayOrderRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `biz_type`, `biz_id`, `user_id`, `total_cents`, `status`, \
                    `wechat_transaction_id`, `created_month` \
             FROM `payment_order` WHERE `order_no` = ? FOR UPDATE",
    )
    .bind(merchant_order_no)
    .fetch_optional(&mut *conn)
    .await
}

/// 锁读投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type IntentRow = (
    String,
    u64,
    u64,
    String,
    i64,
    i64,
    u8,
    u16,
    i64,
    String,
    Option<u64>,
    chrono::DateTime<chrono::Utc>,
    u64,
    i64,
);

/// 锁定支付意图并核对归属、付款人与金额（核对在调用方）。
pub async fn lock_intent_tx(
    conn: &mut MySqlConnection,
    merchant_order_no: &str,
) -> Result<Option<IntentRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`intent_id` AS CHAR), `user_id`, `payment_order_id`, CAST(`openid` AS CHAR), \
                    `electric_cents`, `service_cents`, `charge_mode`, `charge_quantity`, \
                    `total_cents`, `port_code`, `charge_order_id`, `expires_at`, \
                    CAST(COALESCE(`coupon_grant_id`, 0) AS UNSIGNED), `discount_cents` \
             FROM `charge_payment_intent` WHERE `merchant_order_no` = ? FOR UPDATE",
    )
    .bind(merchant_order_no)
    .fetch_optional(&mut *conn)
    .await
}

/// 钱包充值分支：支付单条件推进 paid（含 paid_cents 清零前置，RowsAffected 交调用方判定）。
pub async fn mark_wallet_order_paid_tx(
    conn: &mut MySqlConnection,
    order_id: u64,
    paid_cents: i64,
    transaction_id: &str,
    paid_at: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `payment_order` SET `status` = ?, `paid_cents` = ?, \
         `wechat_transaction_id` = ?, `paid_at` = ? \
         WHERE `id` = ? AND `status` = ? AND `paid_cents` = 0",
    )
    .bind(PaymentOrderStatus::Paid)
    .bind(paid_cents)
    .bind(transaction_id)
    .bind(paid_at)
    .bind(order_id as i64)
    .bind(PaymentOrderStatus::Initiated)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
}

/// 充值申请号（钱包入账的幂等键；缺失即存储异常）。
pub async fn recharge_request_id_tx(
    conn: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`request_id` AS CHAR) FROM `wallet_recharge_request` \
         WHERE `payment_order_id` = ? LIMIT 1",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *conn)
    .await
}

/// charge 分支：支付单条件推进 paid（并发竞争失败即冲突，RowsAffected 交调用方判定）。
#[allow(clippy::too_many_arguments)]
pub async fn mark_charge_order_paid_tx(
    conn: &mut MySqlConnection,
    order_id: u64,
    merchant_order_no: &str,
    paid_cents: i64,
    transaction_id: &str,
    paid_at: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `payment_order` SET `status` = ?, `paid_cents` = ?, \
         `wechat_transaction_id` = ?, `paid_at` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ? AND `total_cents` = ? AND `paid_cents` = 0",
    )
    .bind(PaymentOrderStatus::Paid)
    .bind(paid_cents)
    .bind(transaction_id)
    .bind(paid_at)
    .bind(order_id as i64)
    .bind(merchant_order_no)
    .bind(PaymentOrderStatus::Initiated)
    .bind(paid_cents)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
}

/// 迟到重放：意图当前状态（refund_required 时原样返回）。
pub async fn intent_status_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
) -> Result<PaymentIntentStatus, sqlx::Error> {
    sqlx::query_scalar("SELECT `status` FROM `charge_payment_intent` WHERE `intent_id` = ?")
        .bind(intent_id)
        .fetch_one(&mut *conn)
        .await
}

/// 已到账重放：充电订单号（归属与支付单双重匹配）。
pub async fn charge_order_no_tx(
    conn: &mut MySqlConnection,
    charge_order_id: u64,
    payment_order_id: u64,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `order_no` FROM `charge_order` WHERE `id` = ? AND `payment_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .bind(payment_order_id as i64)
    .fetch_one(&mut *conn)
    .await
}

/// 迟到支付：意图条件置 refund_required（RowsAffected 交调用方判定）。
pub async fn mark_intent_refund_required_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ? \
         WHERE `intent_id` = ? AND `status` = ? AND `charge_order_id` IS NULL",
    )
    .bind(PaymentIntentStatus::RefundRequired)
    .bind(intent_id)
    .bind(PaymentIntentStatus::Initiated)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
}

/// 迟到支付：登记自动原路退款。
pub async fn insert_late_refund_tx(
    conn: &mut MySqlConnection,
    refund_no: &str,
    payment_order_id: u64,
    user_id: u64,
    paid_cents: i64,
    reason: &str,
    created_month: chrono::NaiveDate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
         VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, ?)",
    )
    .bind(refund_no)
    .bind(payment_order_id as i64)
    .bind(user_id as i64)
    .bind(BizType::Charge)
    .bind(paid_cents)
    .bind(reason)
    .bind(chargepilot_domain::money::RefundStatus::Pending)
    .bind(chargepilot_domain::money::ExecutionPolicy::Automatic)
    .bind(created_month)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 意图上的设备号（创建充电订单用）。
pub async fn intent_device_id_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT `device_id` FROM `charge_payment_intent` WHERE `intent_id` = ?")
        .bind(intent_id)
        .fetch_one(&mut *conn)
        .await
}

/// 意图上的端口号（创建充电订单用）。
pub async fn intent_port_no_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
) -> Result<u8, sqlx::Error> {
    sqlx::query_scalar("SELECT `port_no` FROM `charge_payment_intent` WHERE `intent_id` = ?")
        .bind(intent_id)
        .fetch_one(&mut *conn)
        .await
}

/// 意图上的冻结快照 JSON（创建订单后冻结到订单）。
pub async fn intent_snapshot_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_payment_intent` WHERE `intent_id` = ?",
    )
    .bind(intent_id)
    .fetch_one(&mut *conn)
    .await
}

/// 绑定业务 ID 与支付单（条件更新，RowsAffected == 1 才有效）。
pub async fn bind_charge_order_tx(
    conn: &mut MySqlConnection,
    order_id: u64,
    charge_order_id: u64,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `payment_order` SET `biz_id` = ? WHERE `id` = ? AND `biz_id` = 0 AND `status` = ?",
    )
    .bind(charge_order_id as i64)
    .bind(order_id as i64)
    .bind(PaymentOrderStatus::Paid)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
}

/// 意图推进 paid 并回填订单（条件更新，RowsAffected 交调用方判定）。
pub async fn mark_intent_paid_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
    paid_at: chrono::DateTime<chrono::Utc>,
    charge_order_id: u64,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ?, `paid_at` = ?, `charge_order_id` = ? \
         WHERE `intent_id` = ? AND `status` = ? AND `charge_order_id` IS NULL",
    )
    .bind(PaymentIntentStatus::Paid)
    .bind(paid_at)
    .bind(charge_order_id as i64)
    .bind(intent_id)
    .bind(PaymentIntentStatus::Initiated)
    .execute(&mut *conn)
    .await
    .map(|r| r.rows_affected())
}
