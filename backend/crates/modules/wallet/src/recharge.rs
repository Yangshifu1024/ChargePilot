//! 充值申请事务：幂等建单、支付单复用与提交后的预支付准备（旧版 recharge.go 对齐）。
//! 锁序为 用户→钱包→请求；READ COMMITTED 使等锁后的重放能读取前次提交的申请；
//! 预支付只在事务提交后请求，渠道失败保留申请，相同 request_id 可恢复支付参数。

use super::{WalletError, account};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, MySqlPool, MySqlConnection};

/// 充值支付能力（旧版 RechargePayments）：实现由 payment 模块注入（批 3 接线），
/// 钱包不引用支付实现。
pub trait RechargePayments: Send + Sync {
    /// 在事务中读取 paymentID 的支付信息；无记录返回 NotFound。
    fn load_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        payment_id: u64,
    ) -> futures_bounds::BoxFuture<'a, Result<RechargePayment, WalletError>>;
    /// 在事务中为 userID 创建 amountCents（分）的充值支付；调用方负责提交。
    fn create_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        user_id: u64,
        amount_cents: i64,
    ) -> futures_bounds::BoxFuture<'a, Result<RechargePayment, WalletError>>;
    /// 在申请已提交后为支付单请求或复用预支付参数。
    /// 失败保留原单号，参数为空时必须返回错误。
    fn prepare<'a>(
        &'a self,
        pay: RechargePayment,
        open_id: &'a str,
    ) -> futures_bounds::BoxFuture<'a, Result<serde_json::Value, WalletError>>;
}

/// dyn 兼容的 Future 别名（与 events 层 EventDispatcher 同一模式）。
pub mod futures_bounds {
    pub type BoxFuture<'a, T> =
        std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

/// 返回支付编号、金额、状态与截止时间（旧版 RechargePayment）。
#[derive(Debug, Clone)]
pub struct RechargePayment {
    pub id: u64,
    /// 商户单号，预支付重试不改变。
    pub order_no: String,
    pub amount_cents: i64,
    /// 支付单状态字面量（initiated/paid/...，domain::money::PaymentStatus 词表）。
    pub status: String,
    /// UTC 支付截止，None 表示没有有效期限。
    pub expires_at: Option<DateTime<Utc>>,
}

/// 充值请求内容（幂等存档 JSON，旧版 RechargeInput）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RechargeInput {
    pub request_id: String,
    /// 申请充值金额，分，100–5000000。
    pub amount_cents: i64,
}

/// 可公开的申请回执（旧版 RechargeResult；字段名与旧版 JSON 一致）。
#[derive(Debug, Clone, Serialize)]
pub struct RechargeResult {
    pub request_id: String,
    pub payment_order_id: u64,
    pub merchant_order_no: String,
    pub amount_cents: i64,
    pub status: String,
    pub can_pay: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_params: Option<serde_json::Value>,
}

/// 充值申请服务（旧版 RechargeService）。
pub struct RechargeService {
    pub db: MySqlPool,
    pub payments: std::sync::Arc<dyn RechargePayments>,
}

impl RechargeService {
    /// 创建用户充值申请与支付记录，失败全部回滚（旧版 RechargeService.Create）。
    /// 渠道失败返回 PrepayUnavailable，保留申请，相同 request_id 可恢复支付参数。
    pub async fn create(
        &self,
        user_id: u64,
        input: &RechargeInput,
    ) -> Result<RechargeResult, WalletError> {
        if user_id == 0
            || !is_uuid(&input.request_id)
            || !(100..=5_000_000).contains(&input.amount_cents)
        {
            return Err(WalletError::RequestConflict);
        }
        // READ COMMITTED：使等锁后的重放能读取前次提交的申请（旧版 TxOptions 语义）。
        let mut conn = self.db.acquire().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let open_id = account::find_user_openid_tx(&mut tx, user_id).await?;
        if open_id.is_empty() {
            return Err(WalletError::Identity);
        }
        account::lock_user_tx(&mut tx, user_id).await?;
        let account_row = match account::lock_wallet_account_tx(&mut tx, user_id).await {
            Ok(row) => Some(row),
            Err(WalletError::NotFound) => {
                account::open_wallet_account_tx(&mut tx, user_id).await?;
                None
            }
            Err(e) => return Err(e),
        };
        if let Some(row) = account_row {
            if row.status != super::AccountStatus::Active {
                return Err(WalletError::Frozen);
            }
        }
        // 幂等重放：同请求号读取既有申请，归属与金额一致则复用支付单。
        let prior: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT `user_id`, `amount_cents`, `payment_order_id` FROM `wallet_recharge_request` \
             WHERE `request_id` = ? LIMIT 1 FOR UPDATE",
        )
        .bind(&input.request_id)
        .fetch_optional(&mut *tx)
        .await?;
        let pay = match prior {
            Some((uid, amount, payment_id)) => {
                if uid != user_id as i64 || amount != input.amount_cents {
                    return Err(WalletError::RequestConflict);
                }
                self.payments.load_in_tx(&mut tx, payment_id as u64).await?
            }
            None => {
                let pay = self
                    .payments
                    .create_in_tx(&mut tx, user_id, input.amount_cents)
                    .await?;
                let payload = serde_json::to_string(input)
                    .map_err(|e| WalletError::Storage(e.to_string()))?;
                sqlx::query(
                    "INSERT INTO `wallet_recharge_request` (`request_id`, `user_id`, \
                     `amount_cents`, `payment_order_id`, `request_json`) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&input.request_id)
                .bind(user_id as i64)
                .bind(input.amount_cents)
                .bind(pay.id as i64)
                .bind(&payload)
                .execute(&mut *tx)
                .await?;
                pay
            }
        };
        tx.commit().await?;
        let can_pay = pay.status == "initiated"
            && pay
                .expires_at
                .is_some_and(|t| t > chrono::Utc::now());
        let mut out = RechargeResult {
            request_id: input.request_id.clone(),
            payment_order_id: pay.id,
            merchant_order_no: pay.order_no.clone(),
            amount_cents: pay.amount_cents,
            status: pay.status.clone(),
            can_pay,
            payment_params: None,
        };
        if can_pay {
            out.payment_params = Some(self.payments.prepare(pay, &open_id).await.map_err(|e| {
                tracing::warn!(payment_order_id = out.payment_order_id, error = %e, "预支付失败");
                WalletError::PrepayUnavailable
            })?);
        }
        Ok(out)
    }
}

/// 客户端请求号须为合法 UUID（旧版 uuid.Validate）。
pub fn is_uuid(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::is_uuid;

    #[test]
    fn uuid_validation() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000g"));
    }
}
