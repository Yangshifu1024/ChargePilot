//! 钱包充值支付桥的表访问：payment_order / charge_prepay（蓝图第五章 repo 层）。

use chargepilot_domain::money::{BizType, PayMethod, PaymentOrderStatus};
use sqlx::MySqlConnection;

/// 支付单读投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type RechargeOrderRow = (
    u64,
    String,
    i64,
    PaymentOrderStatus,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// 事务内读支付单（钱包所需投影）。
pub async fn load_recharge_order_tx(
    tx: &mut MySqlConnection,
    payment_id: u64,
) -> Result<Option<RechargeOrderRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `order_no`, `total_cents`, `status`, `expired_at` \
             FROM `payment_order` WHERE `id` = ?",
    )
    .bind(payment_id as i64)
    .fetch_optional(&mut *tx)
    .await
}

/// 创建充值支付单（initiated），返回主键。
pub async fn insert_recharge_order_tx(
    tx: &mut MySqlConnection,
    order_no: &str,
    user_id: u64,
    amount_cents: i64,
    expires_at: chrono::DateTime<chrono::Utc>,
    month: chrono::NaiveDate,
) -> Result<u64, sqlx::Error> {
    let payment_id: u64 = sqlx::query(
        "INSERT INTO `payment_order` \
         (`order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, \
          `status`, `expired_at`, `created_month`) \
         VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order_no)
    .bind(BizType::WalletRecharge)
    .bind(user_id as i64)
    .bind(PayMethod::Wechat)
    .bind(amount_cents)
    .bind(PaymentOrderStatus::Initiated)
    .bind(expires_at)
    .bind(month)
    .execute(&mut *tx)
    .await?
    .last_insert_id() as u64;
    Ok(payment_id)
}
