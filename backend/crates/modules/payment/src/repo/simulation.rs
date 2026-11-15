//! 模拟支付确认的表访问：payment_order / charge_prepay /
//! charge_payment_intent（蓝图第五章 repo 层）。

use chargepilot_domain::money::PayMethod;
use sqlx::MySqlPool;

/// 模拟确认读投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type SimulationPayRow = (
    u64,
    String,
    i64,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    String,
);

/// 读取支付单及付款人身份（旧版 FindDevelopmentPayment）。
pub async fn find_development_payment(
    pool: &MySqlPool,
    order_no: &str,
    user_id: u64,
) -> Result<Option<SimulationPayRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.`id`, p.`biz_type`, p.`total_cents`, CAST(u.`openid` AS CHAR), p.`created_at`, p.`status` \
         FROM `payment_order` p JOIN `user` u ON u.`id` = p.`user_id` \
         WHERE p.`order_no` = ? AND p.`user_id` = ? AND p.`pay_method` = ? LIMIT 1",
    )
    .bind(order_no)
    .bind(user_id as i64)
    .bind(PayMethod::Wechat)
    .fetch_optional(pool)
    .await
}

/// 校验支付单经模拟渠道预支付（旧版 provider == "simulation" 判定）。
pub async fn prepay_is_simulation(
    pool: &MySqlPool,
    payment_order_id: u64,
) -> Result<bool, sqlx::Error> {
    let params = crate::repo::find_prepay_params(pool, payment_order_id).await?;
    Ok(params
        .as_deref()
        .map(|p| p.contains("\"simulation\""))
        .unwrap_or(false))
}

/// 内部确认：按商户单号读意图（意图 ID、应付额与付款人）。
pub async fn find_intent_payment(
    pool: &MySqlPool,
    merchant_order_no: &str,
) -> Result<Option<(String, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(i.`intent_id` AS CHAR), p.`total_cents`, CAST(i.`openid` AS CHAR) \
         FROM `charge_payment_intent` i \
         JOIN `payment_order` p ON p.`id` = i.`payment_order_id` \
         WHERE i.`merchant_order_no` = ? LIMIT 1",
    )
    .bind(merchant_order_no)
    .fetch_optional(pool)
    .await
}
