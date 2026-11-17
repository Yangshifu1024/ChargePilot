//! 下单冻结的表访问：charge_payment_intent / payment_order / user（蓝图第五章 repo 层）。

use chargepilot_domain::money::{BizType, PayMethod, PaymentOrderStatus};
use chargepilot_domain::payment::PaymentIntentStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 意图冻结投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type FrozenIntentRow = (
    String,
    String,
    u64,
    String,
    u8,
    u64,
    i64,
    i64,
    i64,
    i64,
    u16,
    u16,
    u64,
    u64,
    chrono::DateTime<chrono::Utc>,
    serde_json::Value,
);

/// 同（user, request）的意图恢复读取（逐项核对在调用方）。
pub async fn find_frozen_intent(
    pool: &MySqlPool,
    user_id: u64,
    client_request_id: &str,
) -> Result<Option<FrozenIntentRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(i.`intent_id` AS CHAR), i.`merchant_order_no`, i.`station_id`, i.`device_id`, \
                i.`port_no`, i.`offer_id`, i.`discount_cents`, i.`electric_cents`, \
                i.`service_cents`, i.`total_cents`, i.`estimated_minutes`, \
                i.`charge_quantity`, i.`payment_order_id`, CAST(COALESCE(i.`coupon_grant_id`, 0) AS UNSIGNED), \
                i.`expires_at`, i.`pricing_snapshot` \
         FROM `charge_payment_intent` i \
         WHERE i.`user_id` = ? AND i.`client_request_id` = ? AND i.`status` = ? LIMIT 1",
    )
    .bind(user_id as i64)
    .bind(client_request_id)
    .bind(PaymentIntentStatus::Initiated)
    .fetch_optional(pool)
    .await
}

/// 仅释放本端口已过期的预占，与新意图创建同事务。
pub async fn expire_stale_intents_tx(
    conn: &mut MySqlConnection,
    port_code: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ? \
         WHERE `port_code` = ? AND `status` = ? AND `expires_at` <= UTC_TIMESTAMP(3)",
    )
    .bind(PaymentIntentStatus::Expired)
    .bind(port_code)
    .bind(PaymentIntentStatus::Initiated)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 端口可用性：有效预占或进行中订单存在即冲突。
pub async fn port_busy_count_tx(
    conn: &mut MySqlConnection,
    port_code: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_payment_intent` i \
         LEFT JOIN `charge_order` c ON c.`id` = i.`charge_order_id` \
         WHERE i.`port_code` = ? AND ((i.`status` = ? AND i.`expires_at` > UTC_TIMESTAMP(3)) \
           OR (i.`status` = ? AND c.`status` IN (?, ?)))",
    )
    .bind(port_code)
    .bind(PaymentIntentStatus::Initiated)
    .bind(PaymentIntentStatus::Paid)
    .bind(chargepilot_domain::order::ChargeOrderStatus::Paid)
    .bind(chargepilot_domain::order::ChargeOrderStatus::Charging)
    .fetch_one(&mut *conn)
    .await
}

/// 付款人微信身份（不公开；varbinary 列读取侧 CAST AS CHAR）。
pub async fn user_openid_tx(
    conn: &mut MySqlConnection,
    user_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT CAST(`openid` AS CHAR) FROM `user` \
         WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *conn)
    .await?
    .flatten())
}

/// 创建支付单（initiated），返回主键。
pub async fn insert_pay_order_tx(
    conn: &mut MySqlConnection,
    merchant_order_no: &str,
    user_id: u64,
    payable: i64,
    expires_at: chrono::DateTime<chrono::Utc>,
    month: chrono::NaiveDate,
) -> Result<u64, sqlx::Error> {
    let payment_id: u64 = sqlx::query(
        "INSERT INTO `payment_order` \
         (`order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, \
          `status`, `expired_at`, `created_month`) \
         VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?)",
    )
    .bind(merchant_order_no)
    .bind(BizType::Charge)
    .bind(user_id as i64)
    .bind(PayMethod::Wechat)
    .bind(payable)
    .bind(PaymentOrderStatus::Initiated)
    .bind(expires_at)
    .bind(month)
    .execute(&mut *conn)
    .await?
    .last_insert_id() as u64;
    Ok(payment_id)
}

/// 冻结支付意图（端口唯一键冲突 23000 原样上抛由调用方判定）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_intent_tx(
    conn: &mut MySqlConnection,
    intent_id: &str,
    client_request_id: &str,
    merchant_order_no: &str,
    payment_order_id: u64,
    user_id: u64,
    openid: &str,
    device_id: &str,
    port_no: u8,
    port_code: &str,
    station_id: u64,
    pricing_rule_id: u64,
    pricing_rule_version: u64,
    snapshot_json: &str,
    estimated_kwh: &str,
    estimated_minutes: u16,
    electric_cents: i64,
    service_cents: i64,
    total_cents: i64,
    charge_mode: u8,
    charge_quantity: u16,
    offer_id: u64,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_payment_intent` \
         (`intent_id`, `client_request_id`, `merchant_order_no`, `payment_order_id`, `user_id`, \
          `openid`, `device_id`, `port_no`, `port_code`, `station_id`, `pricing_rule_id`, \
          `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, `estimated_minutes`, \
          `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, `charge_quantity`, \
          `status`, `expires_at`, `offer_id`) \
         VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(intent_id)
    .bind(client_request_id)
    .bind(merchant_order_no)
    .bind(payment_order_id as i64)
    .bind(user_id as i64)
    .bind(openid)
    .bind(device_id)
    .bind(port_no)
    .bind(port_code)
    .bind(station_id as i64)
    .bind(pricing_rule_id as i64)
    .bind(pricing_rule_version as i64)
    .bind(snapshot_json)
    .bind(estimated_kwh)
    .bind(estimated_minutes as i64)
    .bind(electric_cents)
    .bind(service_cents)
    .bind(total_cents)
    .bind(charge_mode)
    .bind(charge_quantity as i64)
    .bind(PaymentIntentStatus::Initiated)
    .bind(expires_at)
    .bind(offer_id as i64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
