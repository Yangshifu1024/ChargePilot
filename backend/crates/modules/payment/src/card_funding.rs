//! 刷卡支付的资金侧表访问（旧版 payment/card_funding_repo.go 与端口占用校验）。
//! 首购写入已付余额支付单与意图，续购累计已付金额；钱包实际扣款与这些写入
//! 由 card 用例在同一事务原子提交，本模块不扣钱包、不调用支付渠道。

use crate::channel::futures_bounds::BoxFuture;
use chargepilot_domain::money::PaymentStatus;

/// 卡续购没有匹配到已付余额支付单（旧版 ErrCardFundingConflict）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFundingConflict;

/// 在事务内创建刷卡首购的已付余额支付单，返回支付单主键（旧版 CreateCardPaymentInTx）。
pub async fn create_card_payment_in_tx(
    tx: &mut sqlx::MySqlConnection,
    payment_order_id: u64,
    order_no: &str,
    user_id: u64,
    total_cents: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, \
         `pay_method`, `total_cents`, `paid_cents`, `status`, `paid_at`, `created_month`) \
         VALUES (?, ?, 'charge', 0, ?, 'balance', ?, ?, 'paid', UTC_TIMESTAMP(3), ?)",
    )
    .bind(payment_order_id as i64)
    .bind(order_no)
    .bind(user_id as i64)
    .bind(total_cents)
    .bind(total_cents)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 在事务内保存刷卡支付申请及冻结快照（旧版 CreateCardIntentInTx）。
/// openid 为绑定用户的微信标识（intent 列非空）；charge_order_id 首购同事务回填。
#[allow(clippy::too_many_arguments)]
pub async fn create_card_intent_in_tx(
    tx: &mut sqlx::MySqlConnection,
    intent_id: &str,
    client_request_id: &str,
    merchant_order_no: &str,
    payment_order_id: u64,
    user_id: u64,
    openid: &[u8],
    device_id: &str,
    port_no: u8,
    port_code: &str,
    station_id: u64,
    pricing_rule_id: u64,
    pricing_rule_version: u32,
    pricing_snapshot: &serde_json::Value,
    estimated_minutes: u16,
    total_cents: i64,
    charge_quantity: u16,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `merchant_order_no`, \
         `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, `port_code`, `station_id`, \
         `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, \
         `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, \
         `charge_quantity`, `status`, `expires_at`, `offer_id`, `charge_order_id`) \
         VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, ?, ?, ?, ?, ?, CAST(? AS JSON), '0.000', \
         ?, 0, 0, ?, 4, ?, 'paid', DATE_ADD(UTC_TIMESTAMP(3), INTERVAL 5 MINUTE), ?, ?)",
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
    .bind(pricing_rule_version)
    .bind(pricing_snapshot.to_string())
    .bind(estimated_minutes)
    .bind(total_cents)
    .bind(charge_quantity)
    .bind((pricing_rule_id * 100 + 1) as i64)
    .bind(charge_order_id as i64)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 在事务内为支付单回填充电订单关联（旧版 SetCardOrderInTx）。
pub async fn set_card_order_in_tx(
    tx: &mut sqlx::MySqlConnection,
    payment_order_id: u64,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `payment_order` SET `biz_id` = ? WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .bind(payment_order_id as i64)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 在事务内为已付余额单累计续购金额（旧版 ExtendCardPaymentInTx）。
/// 只匹配 balance 已付或部分退款支付单；未命中返回冲突（钱包扣款由调用方回滚）。
pub async fn extend_card_payment_in_tx(
    tx: &mut sqlx::MySqlConnection,
    payment_order_id: u64,
    cents: i64,
) -> Result<(), CardFundingConflict> {
    let n = sqlx::query(
        "UPDATE `payment_order` SET `paid_cents` = `paid_cents` + ?, `total_cents` = `total_cents` + ? \
         WHERE `id` = ? AND `pay_method` = 'balance' AND `status` IN (?, ?)",
    )
    .bind(cents)
    .bind(cents)
    .bind(payment_order_id as i64)
    .bind(PaymentStatus::PAID)
    .bind(PaymentStatus::PARTIAL_REFUNDED)
    .execute(&mut *tx)
    .await
    .map_err(|_| CardFundingConflict)?
    .rows_affected();
    if n != 1 {
        return Err(CardFundingConflict);
    }
    Ok(())
}

/// 端口占用校验（旧版 CheckoutPortAvailable）：存在未过期申请或已付未完单的
/// 占用返回 false；并发创建仍由端口唯一键兜底（本查询不加锁）。
pub fn checkout_port_available<'a>(
    tx: &'a mut sqlx::MySqlConnection,
    port_code: &'a str,
) -> BoxFuture<'a, Result<bool, sqlx::Error>> {
    Box::pin(async move {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM `charge_payment_intent` i \
             LEFT JOIN `charge_order` c ON c.`id` = i.`charge_order_id` \
             WHERE i.`port_code` = ? AND ((i.`status` = 'initiated' AND i.`expires_at` > UTC_TIMESTAMP(3)) \
               OR (i.`status` = 'paid' AND c.`status` IN ('paid', 'starting', 'charging', 'stopping')))",
        )
        .bind(port_code)
        .fetch_one(&mut *tx)
        .await?;
        Ok(count == 0)
    })
}
