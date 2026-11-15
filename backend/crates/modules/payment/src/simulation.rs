//! 模拟支付确认（旧版 simulation_repo.go 对齐）。仅在 PAYMENT_MODE=simulation
//! 装配；确认仍走支付成功事务（apply::apply_payment），用户与金额取自原支付单，
//! 交易号为 SIM 前缀稳定编号保证幂等重放。

use super::apply::{ApplyResult, VerifiedPayment, apply_payment};
use crate::error::SimulationError;
use sqlx::MySqlPool;

/// 读取支付单及付款人身份（旧版 FindDevelopmentPayment）。
async fn find_development_payment(
    pool: &MySqlPool,
    order_no: &str,
    user_id: u64,
) -> Result<
    (
        u64,
        String,
        i64,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        String,
    ),
    SimulationError,
> {
    // 模拟确认读投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type SimulationPayRow = (
        u64,
        String,
        i64,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        String,
    );
    let row: Option<SimulationPayRow> = sqlx::query_as(
            "SELECT p.`id`, p.`biz_type`, p.`total_cents`, CAST(u.`openid` AS CHAR), p.`created_at`, p.`status` \
             FROM `payment_order` p JOIN `user` u ON u.`id` = p.`user_id` \
             WHERE p.`order_no` = ? AND p.`user_id` = ? AND p.`pay_method` = ? LIMIT 1",
        )
        .bind(order_no)
        .bind(user_id as i64)
        .bind(chargepilot_domain::money::PayMethod::Wechat)
        .fetch_optional(pool)
        .await?;
    let Some((id, biz_type, total_cents, openid, created_at, status)) = row else {
        return Err(SimulationError::NotFound);
    };
    Ok((id, biz_type, total_cents, openid, created_at, status))
}

/// 校验支付单经模拟渠道预支付（旧版 provider == "simulation" 判定）。
async fn ensure_simulation(pool: &MySqlPool, payment_order_id: u64) -> Result<(), SimulationError> {
    let params: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`params_json` AS CHAR) FROM `charge_prepay` WHERE `payment_order_id` = ?",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(pool)
    .await?;
    let tagged = params
        .as_deref()
        .map(|p| p.contains("\"simulation\""))
        .unwrap_or(false);
    if !tagged {
        return Err(SimulationError::NotSimulation);
    }
    Ok(())
}

/// 当前用户确认自己的模拟支付（旧版 ConfirmUser）。
pub async fn confirm_user(
    pool: &MySqlPool,
    user_id: u64,
    merchant_order_no: &str,
) -> Result<ApplyResult, SimulationError> {
    let (payment_order_id, _biz_type, total_cents, openid, created_at, _status) =
        find_development_payment(pool, merchant_order_no, user_id).await?;
    ensure_simulation(pool, payment_order_id).await?;
    let payment = VerifiedPayment {
        merchant_order_no: merchant_order_no.to_string(),
        // 稳定交易号：同单重放同报文（旧版 SIMH5 前缀）。
        transaction_id: format!("SIMH5{merchant_order_no}"),
        open_id: openid.unwrap_or_default(),
        paid_cents: total_cents,
        paid_at_unix_micros: created_at.timestamp_micros(),
    };
    Ok(apply_payment(pool, &payment).await?)
}

/// 内部确认（旧版 ConfirmIntent）：按商户单号读意图，构造稳定模拟交易。
/// 报文金额取支付单应付额（旧版 intent.TotalCents 在无券时与应付额相同；
/// 引入券冻结后应付额 = 总额 - 优惠，与真实渠道回调金额语义一致）。
pub async fn confirm_intent(
    pool: &MySqlPool,
    merchant_order_no: &str,
) -> Result<ApplyResult, SimulationError> {
    let row: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT CAST(i.`intent_id` AS CHAR), p.`total_cents`, CAST(i.`openid` AS CHAR) \
         FROM `charge_payment_intent` i \
         JOIN `payment_order` p ON p.`id` = i.`payment_order_id` \
         WHERE i.`merchant_order_no` = ? LIMIT 1",
    )
    .bind(merchant_order_no)
    .fetch_optional(pool)
    .await?;
    let Some((intent_id, total_cents, openid)) = row else {
        return Err(SimulationError::NotFound);
    };
    let payment = VerifiedPayment {
        merchant_order_no: merchant_order_no.to_string(),
        transaction_id: format!("SIMTX{intent_id}"),
        open_id: openid.to_string(),
        paid_cents: total_cents,
        paid_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    Ok(apply_payment(pool, &payment).await?)
}
