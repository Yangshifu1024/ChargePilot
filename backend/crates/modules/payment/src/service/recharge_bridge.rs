//! 钱包充值的支付能力实现（旧版 recharge_payment_repo.go 对齐）。
//! 归属：payment_order / charge_prepay 表访问归 payment（repo/recharge_bridge.rs）；
//! 本实现是 wallet::RechargePayments 的唯一生产实现（cmd 装配注入）。

use super::channel::{PayProvider, PrepayRequest};
use crate::repo::{find_prepay_params, insert_ignore_prepay_tx, recharge_bridge as repo};
use chargepilot_domain::money::PaymentOrderStatus;
use chargepilot_mod_wallet::recharge::RechargePayment;
use chargepilot_mod_wallet::recharge::futures_bounds::BoxFuture;
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::MySqlPool;

/// 充值支付桥（旧版 RechargePaymentRepo）。
pub struct RechargePaymentBridge {
    pub pool: MySqlPool,
    pub allocator: SegmentAllocator,
    pub provider: std::sync::Arc<dyn PayProvider>,
}

impl RechargePaymentBridge {
    /// payment_order 行 → 钱包所需投影（旧版 rechargePaymentView）。
    fn view(
        id: u64,
        order_no: String,
        total_cents: i64,
        status: PaymentOrderStatus,
        expired_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> RechargePayment {
        RechargePayment {
            id,
            order_no,
            amount_cents: total_cents,
            status,
            expires_at: expired_at,
        }
    }
}

impl chargepilot_mod_wallet::recharge::RechargePayments for RechargePaymentBridge {
    fn load_in_tx<'a>(
        &'a self,
        tx: &'a mut sqlx::MySqlConnection,
        payment_id: u64,
    ) -> BoxFuture<'a, Result<RechargePayment, chargepilot_mod_wallet::WalletError>> {
        Box::pin(async move {
            repo::load_recharge_order_tx(tx, payment_id)
                .await?
                .map(|(id, no, cents, status, expiry)| Self::view(id, no, cents, status, expiry))
                .ok_or(chargepilot_mod_wallet::WalletError::NotFound)
        })
    }

    fn create_in_tx<'a>(
        &'a self,
        tx: &'a mut sqlx::MySqlConnection,
        user_id: u64,
        amount_cents: i64,
    ) -> BoxFuture<'a, Result<RechargePayment, chargepilot_mod_wallet::WalletError>> {
        Box::pin(async move {
            // 充值支付单号：P + 号段分配 ID（旧版 NewOrderNumber）。
            let id = self
                .allocator
                .next_id()
                .await
                .map_err(|e| chargepilot_mod_wallet::WalletError::Storage(e.to_string()))?;
            let order_no = format!("P{id}");
            let expires_at = chrono::Utc::now() + chrono::Duration::minutes(30);
            let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
            let payment_id = repo::insert_recharge_order_tx(
                tx,
                &order_no,
                user_id,
                amount_cents,
                expires_at,
                month,
            )
            .await?;
            Ok(Self::view(
                payment_id,
                order_no,
                amount_cents,
                PaymentOrderStatus::Initiated,
                Some(expires_at),
            ))
        })
    }

    fn prepare<'a>(
        &'a self,
        pay: RechargePayment,
        open_id: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, chargepilot_mod_wallet::WalletError>> {
        Box::pin(async move {
            let Some(expires_at) = pay.expires_at else {
                return Err(chargepilot_mod_wallet::WalletError::PrepayNotConfigured);
            };
            // 复用已保存参数；缺失时请求渠道并以 payment_order_id 唯一键仲裁保存。
            let existing = find_prepay_params(&self.pool, pay.id).await?;
            let body = match existing {
                Some(saved) => saved,
                None => {
                    let params = self
                        .provider
                        .prepay(PrepayRequest {
                            merchant_order_no: pay.order_no.clone(),
                            open_id: open_id.to_string(),
                            amount_cents: pay.amount_cents,
                            expires_at_unix_micros: expires_at.timestamp_micros(),
                        })
                        .await
                        .map_err(|_| chargepilot_mod_wallet::WalletError::PrepayUnavailable)?;
                    if params.is_empty() {
                        return Err(chargepilot_mod_wallet::WalletError::PrepayUnavailable);
                    }
                    let body = serde_json::to_string(&params)
                        .map_err(|e| chargepilot_mod_wallet::WalletError::Storage(e.to_string()))?;
                    let saved =
                        insert_ignore_prepay_tx(&self.pool, pay.id, &body, &params.prepay_id)
                            .await?;
                    if saved == 1 {
                        body
                    } else {
                        // 并发者已保存：重读首次参数（旧版语义）。
                        find_prepay_params(&self.pool, pay.id).await?.ok_or(
                            chargepilot_mod_wallet::WalletError::Storage(
                                "预支付参数读取失败".to_string(),
                            ),
                        )?
                    }
                }
            };
            serde_json::from_str(&body)
                .map_err(|e| chargepilot_mod_wallet::WalletError::Storage(e.to_string()))
        })
    }
}
