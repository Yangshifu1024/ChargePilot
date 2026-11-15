//! 模拟支付确认（旧版 simulation_repo.go 对齐）。仅在 PAYMENT_MODE=simulation
//! 装配；确认仍走支付成功事务（apply::apply_payment），用户与金额取自原支付单，
//! 交易号为 SIM 前缀稳定编号保证幂等重放。SQL 在 repo/simulation.rs。

use super::apply::{ApplyResult, VerifiedPayment, apply_payment};
use crate::error::SimulationError;
use crate::repo::simulation as repo;
use sqlx::MySqlPool;

/// 当前用户确认自己的模拟支付（旧版 ConfirmUser）。
pub async fn confirm_user(
    pool: &MySqlPool,
    user_id: u64,
    merchant_order_no: &str,
) -> Result<ApplyResult, SimulationError> {
    let Some((payment_order_id, _biz_type, total_cents, openid, created_at, _status)) =
        repo::find_development_payment(pool, merchant_order_no, user_id).await?
    else {
        return Err(SimulationError::NotFound);
    };
    if !repo::prepay_is_simulation(pool, payment_order_id).await? {
        return Err(SimulationError::NotSimulation);
    }
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
    let Some((intent_id, total_cents, openid)) =
        repo::find_intent_payment(pool, merchant_order_no).await?
    else {
        return Err(SimulationError::NotFound);
    };
    let payment = VerifiedPayment {
        merchant_order_no: merchant_order_no.to_string(),
        transaction_id: format!("SIMTX{intent_id}"),
        open_id: openid,
        paid_cents: total_cents,
        paid_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    Ok(apply_payment(pool, &payment).await?)
}
