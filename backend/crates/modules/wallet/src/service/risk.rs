//! 风险审核、资金预占与解冻用例：审核（customer_finance / customer_cs）与
//! 解冻（仅 customer_finance）各自要求实时权限；审核不直接退款：批准走
//! 预占，驳回只留记录；解冻不表示退款完成。SQL 在 repo/risk.rs。

use crate::repo::account;
use crate::repo::risk;
use crate::service::refund::{RefundService, WalletRefundReservation};
use crate::{FreezeStatus, WalletError};
use chargepilot_domain::money::{PriorReview, release_replay, review_replay, valid_review_comment};
use chargepilot_mod_ops::Profile;
use serde::Serialize;
use sqlx::{Acquire, MySqlPool};

/// 需原样存档的复核意见。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RiskReviewInput {
    /// 必须显式给出（反序列化后为 None 表示未填）。
    pub approved: Option<bool>,
    /// 审核依据，1–255 字。
    pub comment: String,
}

/// 解冻依据。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RiskReleaseInput {
    pub comment: String,
}

/// 风险服务。
pub struct RiskService {
    pub db: MySqlPool,
    pub refund: std::sync::Arc<RefundService>,
}

/// 风控列表行（JSON 字段名与契约一致）。
#[derive(Debug, Clone, Serialize)]
pub struct WalletRiskRow {
    pub request_id: String,
    pub user_id: u64,
    pub amount_cents: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: String,
    /// 首次审核回执（JSON 值）；未审核为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<serde_json::Value>,
    /// 首次解冻回执（JSON 值）；未解冻为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_created_at: Option<String>,
    /// 冻结记录状态（frozen/unfrozen）；无冻结链接为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freeze_status: Option<String>,
    pub freeze_linked: bool,
    /// 当前操作者可否审核（角色 + 实时权限）。
    pub can_review: bool,
    /// 当前操作者可否解冻（角色 + 权限 + 已审核未解冻 + 冻结生效中）。
    pub can_release: bool,
}

/// 首次回执 JSON 解析；空值/坏值保留 None。
fn parse_receipt(v: serde_json::Value) -> Option<serde_json::Value> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => serde_json::from_str(&s).ok(),
        serde_json::Value::Bool(b) => Some(serde_json::Value::Bool(b)),
        serde_json::Value::Number(n) => Some(serde_json::Value::Number(n)),
        serde_json::Value::Array(a) => Some(serde_json::Value::Array(a)),
        serde_json::Value::Object(o) => Some(serde_json::Value::Object(o)),
    }
}

impl RiskService {
    /// 按 status/page/size 查询风险申请。
    /// reviewed 查已审核，其余查未审核；按钮资格按 actor 实时权限计算。
    pub async fn list(
        &self,
        actor: &Profile,
        status: &str,
        page: u32,
        size: u32,
    ) -> Result<(Vec<WalletRiskRow>, i64), WalletError> {
        if !actor
            .permissions
            .iter()
            .any(|p| p == "finance.wallet_risk.review")
        {
            return Err(WalletError::Forbidden);
        }
        let offset = (page.max(1) - 1) * size.max(1);
        let reviewed = status == "reviewed";
        let raws = risk::risk_rows(&self.db, reviewed, size, offset).await?;
        let total = risk::risk_count(&self.db, reviewed).await?;
        // 操作资格按角色与实时权限计算。
        let rows: Vec<WalletRiskRow> = raws
            .into_iter()
            .map(|raw| {
                let review = raw
                    .review_json
                    .and_then(|s| parse_receipt(serde_json::Value::String(s)));
                let release = raw
                    .release_json
                    .and_then(|s| parse_receipt(serde_json::Value::String(s)));
                let can_review = (actor.role == "customer_finance" || actor.role == "customer_cs")
                    && actor
                        .permissions
                        .iter()
                        .any(|p| p == "finance.wallet_risk.review");
                let can_release = actor.role == "customer_finance"
                    && actor
                        .permissions
                        .iter()
                        .any(|p| p == "finance.wallet_risk.release")
                    && review.is_some()
                    && release.is_none()
                    && raw.freeze_status.as_deref() == Some(FreezeStatus::Frozen.as_str());
                WalletRiskRow {
                    request_id: raw.request_id,
                    user_id: raw.user_id,
                    amount_cents: raw.amount_cents,
                    reason: raw.reason,
                    created_at: raw.created_at.unwrap_or_default(),
                    review,
                    release,
                    review_created_at: raw.review_created_at,
                    release_created_at: raw.release_created_at,
                    freeze_status: raw.freeze_status,
                    freeze_linked: raw.freeze_linked != 0,
                    can_review,
                    can_release,
                }
            })
            .collect();
        Ok((rows, total))
    }

    /// 保存审核回执；批准时同事务完成资金预占。
    /// 与首次审核不同的重放返回冲突；申请、审核、资金预占与审计同事务。
    pub async fn review(
        &self,
        actor: &Profile,
        request_id: &str,
        input: &RiskReviewInput,
        client_ip: &str,
        request_id_hdr: &str,
    ) -> Result<serde_json::Value, WalletError> {
        if actor.role != "customer_finance" && actor.role != "customer_cs"
            || !actor
                .permissions
                .iter()
                .any(|p| p == "finance.wallet_risk.review")
        {
            return Err(WalletError::Forbidden);
        }
        let Some(approved) = input.approved else {
            return Err(WalletError::RequestConflict);
        };
        if !valid_review_comment(&input.comment) {
            return Err(WalletError::RequestConflict);
        }
        let mut conn = self.db.acquire().await?;
        crate::repo::set_read_committed(&mut conn).await?;
        let mut tx = conn.begin().await?;
        let (user_id, amount_cents) = risk::lock_refund_request(&mut tx, request_id).await?;
        // 幂等重放：同审核人同结论同依据沿用首次回执。
        if let Some(prior) = risk::find_risk_review(&mut tx, request_id).await? {
            let p = PriorReview {
                actor_id: prior.0,
                approved: prior.1,
                comment: prior.2.clone(),
            };
            if review_replay(&p, actor.id, approved, &input.comment).is_err() {
                return Err(WalletError::RequestConflict);
            }
            let response: serde_json::Value =
                serde_json::from_str(&prior.3).map_err(|e| WalletError::Storage(e.to_string()))?;
            tx.commit().await?;
            return Ok(response);
        }
        // 必须恰好关联一条 frequency 冻结。
        let linked = risk::frequency_freeze_count(&mut tx, request_id, user_id).await?;
        if linked != 1 {
            return Err(WalletError::RequestConflict);
        }
        if approved {
            self.refund
                .reserve_in_tx(
                    &mut tx,
                    WalletRefundReservation {
                        request_id: request_id.to_string(),
                        user_id,
                        amount_cents,
                        reason: "wallet risk approved".into(),
                    },
                )
                .await?;
        }
        let response = serde_json::json!({
            "actor_id": actor.id.to_string(),
            "approved": approved,
            "comment": input.comment,
        });
        risk::insert_review_tx(
            &mut tx,
            request_id,
            actor.id,
            approved,
            &input.comment,
            &response,
        )
        .await?;
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            actor,
            "wallet_risk",
            "review",
            "wallet_risk",
            &user_id.to_string(),
            &serde_json::Value::Null,
            &serde_json::json!({"request_id": request_id, "review": response}),
            client_ip,
            request_id_hdr,
        )
        .await?;
        tx.commit().await?;
        Ok(response)
    }

    /// 保存解冻回执并解除频次冻结。
    /// 冻结、钱包状态、回执与审计同事务；无其他生效冻结且用户启用时恢复钱包 active。
    pub async fn release(
        &self,
        actor: &Profile,
        request_id: &str,
        input: &RiskReleaseInput,
        client_ip: &str,
        request_id_hdr: &str,
    ) -> Result<serde_json::Value, WalletError> {
        if actor.role != "customer_finance"
            || !actor
                .permissions
                .iter()
                .any(|p| p == "finance.wallet_risk.release")
        {
            return Err(WalletError::Forbidden);
        }
        if !valid_review_comment(&input.comment) {
            return Err(WalletError::RequestConflict);
        }
        let mut conn = self.db.acquire().await?;
        crate::repo::set_read_committed(&mut conn).await?;
        let mut tx = conn.begin().await?;
        let (user_id, _amount) = risk::lock_refund_request(&mut tx, request_id).await?;
        if let Some(prior) = risk::find_risk_release(&mut tx, request_id).await? {
            let p = PriorReview {
                actor_id: prior.0,
                approved: false,
                comment: prior.1.clone(),
            };
            if release_replay(&p, actor.id, &input.comment).is_err() {
                return Err(WalletError::RequestConflict);
            }
            let response: serde_json::Value =
                serde_json::from_str(&prior.2).map_err(|e| WalletError::Storage(e.to_string()))?;
            tx.commit().await?;
            return Ok(response);
        }
        // 审核须恰好一条。
        let review_count = risk::review_count(&mut tx, request_id).await?;
        if review_count != 1 {
            return Err(WalletError::RequestConflict);
        }
        // 锁序：钱包→冻结记录。
        account::lock_wallet_account_tx(&mut tx, user_id).await?;
        let (freeze_id, freeze_user, freeze_status, trigger_rule) =
            risk::lock_freeze_tx(&mut tx, request_id)
                .await?
                .ok_or(WalletError::NotFound)?;
        if freeze_user != user_id
            || trigger_rule != "wallet_refund_frequency"
            || freeze_status != FreezeStatus::Frozen.as_str()
        {
            return Err(WalletError::RequestConflict);
        }
        risk::unfreeze_tx(&mut tx, freeze_id, FreezeStatus::Unfrozen).await?;
        // 剩余生效冻结数与用户状态分别判定（两列取值串相同但事实不同）。
        let active = risk::active_freeze_count(&mut tx, user_id, FreezeStatus::Frozen).await?;
        let user_active = risk::user_status_active(&mut tx, user_id).await?;
        let restore = active == 0 && user_active;
        if restore {
            // 钱包 ID 从锁内账户行取。
            let wallet_id = risk::wallet_id_by_user_tx(&mut tx, user_id).await?;
            account::activate_wallet_tx(&mut tx, wallet_id).await?;
        }
        let response = serde_json::json!({
            "actor_id": actor.id.to_string(),
            "comment": input.comment,
            "wallet_active": restore,
        });
        risk::insert_release_tx(&mut tx, request_id, actor.id, &input.comment, &response).await?;
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut tx,
            actor,
            "wallet_risk",
            "release",
            "wallet_risk",
            &user_id.to_string(),
            &serde_json::Value::Null,
            &serde_json::json!({"request_id": request_id, "release": response}),
            client_ip,
            request_id_hdr,
        )
        .await?;
        tx.commit().await?;
        Ok(response)
    }
}
