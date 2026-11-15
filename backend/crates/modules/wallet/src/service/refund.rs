//! 余额退款申请与原路拆单预占（旧版 refund.go + wallet_reservation_repo.go 对齐）。
//! 锁序为 钱包→申请→充值支付→在途退款；accepted 表示已预占资金，
//! manual_review 表示待审核，均不表示渠道已退回资金。SQL 在 repo/refund.rs。

use crate::WalletError;
use crate::repo::{account, refund};
use chargepilot_domain::money::{BizType, RefundDraft};
use serde::Serialize;
use sqlx::{Acquire, MySqlConnection, MySqlPool};

/// 可原路退款的充值支付来源（旧版 RechargeSource）。
#[derive(Debug, Clone)]
pub struct RechargeSource {
    pub id: u64,
    /// 充值业务编号。
    pub biz_id: u64,
    /// 渠道已收金额，分。
    pub paid_cents: i64,
    /// 申请金额，分。
    pub total_cents: i64,
    /// 已成功退回金额，分。
    pub refunded_cents: i64,
    /// 是否具有渠道交易号。
    pub transaction_confirmed: bool,
}

/// 在已锁钱包的事务中锁定充值支付（旧版 RechargeSourceReader）。
/// 实现由 payment 模块注入；无来源返回空集合。
pub trait RechargeSourceReader: Send + Sync {
    fn lock_recharge_sources<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        user_id: u64,
    ) -> crate::service::recharge::futures_bounds::BoxFuture<
        'a,
        Result<Vec<RechargeSource>, WalletError>,
    >;
}

/// settlement 提供退款创建及在途聚合（旧版 RefundRecorder）；实现随批 5 接入。
pub trait RefundRecorder: Send + Sync {
    /// 当前读 paymentID 的在途退款金额（分），调用方已锁支付；0 表示无占用。
    fn reserved_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        payment_id: u64,
    ) -> crate::service::recharge::futures_bounds::BoxFuture<'a, Result<i64, WalletError>>;
    /// 在调用方事务中按 draft 创建原单号退款；返回主键。
    /// 记录、钱包冻结和来源分摊必须由调用方一并提交，不在此调用渠道。
    fn create_in_tx<'a>(
        &'a self,
        tx: &'a mut MySqlConnection,
        draft: RefundDraft,
    ) -> crate::service::recharge::futures_bounds::BoxFuture<'a, Result<u64, WalletError>>;
}

/// 用户退款申请输入（旧版 RefundInput）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RefundInput {
    pub request_id: String,
    /// 申请原路退回金额，分，1–5000000。
    pub amount_cents: i64,
    /// 退款依据，最多 255 字，比较前去除首尾空白。
    #[serde(default)]
    pub reason: String,
}

/// 申请回执（旧版以 map 返回；此处定形，JSON 字段名一致）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RefundReceipt {
    pub request_id: String,
    pub status: String,
    pub refund_cents: i64,
}

/// 预占输入（旧版 WalletRefundReservation）。
#[derive(Debug, Clone)]
pub struct WalletRefundReservation {
    pub request_id: String,
    pub user_id: u64,
    pub amount_cents: i64,
    pub reason: String,
}

/// 钱包退款服务（旧版 RefundService）。
pub struct RefundService {
    pub db: MySqlPool,
    pub sources: std::sync::Arc<dyn RechargeSourceReader>,
    pub recorder: std::sync::Arc<dyn RefundRecorder>,
}

impl RefundService {
    /// 提交用户余额退款，重放同一请求时返回首次回执，内容不同报冲突（旧版 Request）。
    /// 申请、频次冻结、拆单与回执同事务。
    pub async fn request(
        &self,
        user_id: u64,
        input: &RefundInput,
    ) -> Result<RefundReceipt, WalletError> {
        if user_id == 0
            || !crate::service::recharge::is_uuid(&input.request_id)
            || input.amount_cents < chargepilot_domain::limits::REFUND_AMOUNT_MIN_CENTS
            || input.amount_cents > 5_000_000
            || input.reason.chars().count() > 255
        {
            return Err(WalletError::RequestConflict);
        }
        let reason = input.reason.trim().to_string();
        let mut conn = self.db.acquire().await?;
        crate::repo::set_read_committed(&mut conn).await?;
        let mut tx = conn.begin().await?;
        // 锁序第一步：钱包行锁。
        let acct = account::lock_wallet_account_tx(&mut tx, user_id).await?;
        // 幂等重放：同请求号逐字段比对后返回首次回执。
        let prior = refund::lock_request_replay_tx(&mut tx, &input.request_id).await?;
        if let Some((prior_user, prior_reason, response_json)) = prior {
            if prior_user != user_id || prior_reason.unwrap_or_default() != reason {
                return Err(WalletError::RequestConflict);
            }
            let receipt: RefundReceipt = serde_json::from_str(&response_json)
                .map_err(|e| WalletError::Storage(e.to_string()))?;
            if receipt.refund_cents != input.amount_cents {
                return Err(WalletError::RequestConflict);
            }
            tx.commit().await?;
            return Ok(receipt);
        }
        if acct.status != crate::AccountStatus::Active {
            return Err(WalletError::Frozen);
        }
        if acct.balance_cents - acct.frozen_cents < input.amount_cents {
            return Err(WalletError::InsufficientBalance);
        }
        let reason_opt = if reason.is_empty() {
            None
        } else {
            Some(reason.clone())
        };
        refund::insert_request_tx(
            &mut tx,
            &input.request_id,
            user_id,
            input.amount_cents,
            &reason_opt,
        )
        .await?;
        // 频次风控：5 分钟窗口内（含本次）第 3 次申请起转人工并冻结钱包。
        let since = chrono::Utc::now() - chrono::Duration::minutes(5);
        let count = refund::request_count_since_tx(&mut tx, user_id, since).await?;
        let status = if count >= 3 {
            let freeze_id = refund::insert_frequency_freeze_tx(&mut tx, user_id).await?;
            refund::link_freeze_tx(&mut tx, &input.request_id, freeze_id).await?;
            account::freeze_wallet_tx(&mut tx, acct.id).await?;
            "manual_review".to_string()
        } else {
            self.reserve_in_tx(
                &mut tx,
                WalletRefundReservation {
                    request_id: input.request_id.clone(),
                    user_id,
                    amount_cents: input.amount_cents,
                    reason: reason.clone(),
                },
            )
            .await?;
            "accepted".to_string()
        };
        let receipt = RefundReceipt {
            request_id: input.request_id.clone(),
            status,
            refund_cents: input.amount_cents,
        };
        // response_json 为 json 列：绑定 Value（String 绑定会 3140）。
        let response_json =
            serde_json::to_value(&receipt).map_err(|e| WalletError::Storage(e.to_string()))?;
        refund::save_response_tx(&mut tx, &input.request_id, &response_json).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// 按充值来源拆分退款，在调用方事务中冻结可用余额（旧版 ReserveInTx）。
    /// 冻结、退款与来源分摊一并提交；渠道结果未知时保留编号及冻结额。
    /// 调用方须已持钱包锁（风控批准路径在冻结账户上继续预占，故不在本函数锁钱包）。
    pub async fn reserve_in_tx(
        &self,
        tx: &mut MySqlConnection,
        req: WalletRefundReservation,
    ) -> Result<(), WalletError> {
        // 事务内重读钱包余额（旧版在预占函数内加锁读取）。
        let (wallet_id, balance, frozen) = refund::lock_wallet_balance_tx(tx, req.user_id)
            .await?
            .ok_or(WalletError::NotFound)?;
        if req.amount_cents <= 0 || balance < frozen || req.amount_cents > balance - frozen {
            return Err(WalletError::RequestConflict);
        }
        let existing = refund::part_count_tx(tx, &req.request_id).await?;
        if existing != 0 {
            return Err(WalletError::RequestConflict);
        }
        let payments = self.sources.lock_recharge_sources(tx, req.user_id).await?;
        let mut left = req.amount_cents;
        let now = chrono::Utc::now();
        for pay in payments {
            if !pay.transaction_confirmed || pay.paid_cents != pay.total_cents {
                continue;
            }
            let reserved = self.recorder.reserved_in_tx(tx, pay.id).await?;
            let amount = std::cmp::min(left, pay.paid_cents - pay.refunded_cents - reserved);
            if amount <= 0 {
                continue;
            }
            // 稳定分笔退款号：WR + sha256(request:payment) 前 16 字节（旧版逐字节一致）。
            let digest = {
                use sha2_digest::Digest as _;
                let mut h = sha2_digest::Sha256::new();
                sha2_digest::Digest::update(&mut h, req.request_id.as_bytes());
                sha2_digest::Digest::update(&mut h, b":");
                sha2_digest::Digest::update(&mut h, pay.id.to_string().as_bytes());
                let out = sha2_digest::Digest::finalize(h);
                out.iter()
                    .take(16)
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            };
            let refund_id = self
                .recorder
                .create_in_tx(
                    tx,
                    RefundDraft {
                        refund_no: format!("WR{digest}"),
                        payment_order_id: pay.id,
                        user_id: req.user_id,
                        biz_type: BizType::WalletRecharge,
                        biz_id: pay.biz_id,
                        amount_cents: amount,
                        reason: req.reason.clone(),
                        created_month: chargepilot_domain::money::RefundDraft::month_start_of(now),
                        next_attempt_at: Some(now),
                    },
                )
                .await?;
            refund::insert_part_tx(tx, refund_id, &req.request_id, wallet_id, amount).await?;
            left -= amount;
            if left == 0 {
                break;
            }
        }
        if left != 0 {
            return Err(WalletError::RequestConflict);
        }
        refund::freeze_balance_tx(tx, wallet_id, req.amount_cents).await?;
        Ok(())
    }
}

/// sha2 摘要（保持与旧版哈希字节一致：WR + sha256 前 16 字节小写 hex）。
mod sha2_digest {
    pub use sha2::{Digest, Sha256};
}
