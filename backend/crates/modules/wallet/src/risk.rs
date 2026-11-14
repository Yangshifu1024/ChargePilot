//! 风险审核、资金预占与解冻事务（旧版 risk.go + admin_wallet_risk_repo.go 对齐）。
//! 审核（customer_finance / customer_cs）与解冻（仅 customer_finance）各自要求实时权限；
//! 审核不直接退款：批准走 ReserveInTx 预占，驳回只留记录；解冻不表示退款完成。

use super::{
    WalletError, account,
    refund::{RefundService, WalletRefundReservation},
};
use chargepilot_domain::money::{PriorReview, release_replay, review_replay, valid_review_comment};
use chargepilot_mod_ops::Profile;
use serde::Serialize;
use sqlx::{Acquire, MySqlConnection, MySqlPool};

/// 需原样存档的复核意见（旧版 RiskReviewInput）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RiskReviewInput {
    /// 必须显式给出（反序列化后为 None 表示未填）。
    pub approved: Option<bool>,
    /// 审核依据，1–255 字。
    pub comment: String,
}

/// 解冻依据（旧版 RiskReleaseInput）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RiskReleaseInput {
    pub comment: String,
}

/// 风险服务（旧版 RiskService）。
pub struct RiskService {
    pub db: MySqlPool,
    pub refund: std::sync::Arc<RefundService>,
}

/// 风控列表行（旧版 map 输出定形；JSON 字段名与旧版一致）。
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

/// 数据库原始行（review/release 的 response_json 以字符串抓取后解析）。
#[derive(Debug, sqlx::FromRow)]
struct WalletRiskRawRow {
    request_id: String,
    user_id: u64,
    amount_cents: i64,
    reason: Option<String>,
    created_at: Option<String>,
    review_json: Option<String>,
    review_created_at: Option<String>,
    release_json: Option<String>,
    release_created_at: Option<String>,
    freeze_status: Option<String>,
    freeze_linked: i64,
}

impl RiskService {
    /// 按 status/page/size 查询风险申请（旧版 List）。
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
        let null_clause = if reviewed { "IS NOT NULL" } else { "IS NULL" };
        let list_sql = format!(
            "SELECT CAST(r.`request_id` AS CHAR) AS `request_id`, r.`user_id`, r.`amount_cents`, CAST(r.`reason` AS CHAR) AS `reason`, \
                    DATE_FORMAT(r.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                    CAST(v.`response_json` AS CHAR) AS `review_json`, \
                    DATE_FORMAT(v.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `review_created_at`, \
                    CAST(x.`response_json` AS CHAR) AS `release_json`, \
                    DATE_FORMAT(x.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `release_created_at`, \
                    f.`status` AS `freeze_status`, \
                    (l.`freeze_id` IS NOT NULL) AS `freeze_linked` \
             FROM `wallet_refund_request` r \
             LEFT JOIN `wallet_risk_review` v ON v.`request_id` = r.`request_id` \
             LEFT JOIN `wallet_risk_freeze_link` l ON l.`request_id` = r.`request_id` \
             LEFT JOIN `risk_freeze_log` f ON f.`id` = l.`freeze_id` \
             LEFT JOIN `wallet_risk_release` x ON x.`request_id` = r.`request_id` \
             WHERE l.`freeze_id` IS NOT NULL AND v.`request_id` {null_clause} \
             ORDER BY r.`created_at` DESC, r.`request_id` LIMIT ? OFFSET ?"
        );
        let raws = sqlx::query_as::<_, WalletRiskRawRow>(&list_sql)
            .bind(size.max(1))
            .bind(offset)
            .fetch_all(&self.db)
            .await?;
        let count_sql = format!(
            "SELECT COUNT(*) FROM `wallet_refund_request` r \
             LEFT JOIN `wallet_risk_review` v ON v.`request_id` = r.`request_id` \
             JOIN `wallet_risk_freeze_link` l ON l.`request_id` = r.`request_id` \
             WHERE v.`request_id` {null_clause}"
        );
        let (total,): (i64,) = sqlx::query_as(&count_sql).fetch_one(&self.db).await?;
        // 操作资格按角色与实时权限计算（旧版语义）。
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
                    && raw.freeze_status.as_deref() == Some("frozen");
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

    /// 保存审核回执；批准时同事务完成资金预占（旧版 Review）。
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
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let (user_id, amount_cents): (u64, i64) = lock_refund_request(&mut tx, request_id).await?;
        // 幂等重放：同审核人同结论同依据沿用首次回执。
        if let Some(prior) = find_risk_review(&mut tx, request_id).await? {
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
        // 必须恰好关联一条 frequency 冻结（旧版 CountFrequencyFreezes）。
        let (linked,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM `wallet_risk_freeze_link` l \
             JOIN `risk_freeze_log` f ON f.`id` = l.`freeze_id` \
             WHERE l.`request_id` = ? AND f.`user_id` = ? \
               AND f.`trigger_rule` = 'wallet_refund_frequency'",
        )
        .bind(request_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
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
        // response_json 为 json 列：绑定 Value。
        let response_json = response.clone();
        sqlx::query(
            "INSERT INTO `wallet_risk_review` (`request_id`, `actor_id`, `approved`, \
             `comment`, `response_json`) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(actor.id as i64)
        .bind(approved)
        .bind(&input.comment)
        .bind(&response_json)
        .execute(&mut *tx)
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

    /// 保存解冻回执并解除频次冻结（旧版 Release）。
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
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let (user_id, _amount): (u64, i64) = lock_refund_request(&mut tx, request_id).await?;
        if let Some(prior) = find_risk_release(&mut tx, request_id).await? {
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
        // 审核须恰好一条（旧版 CountRiskReviews）。
        let (review_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM `wallet_risk_review` WHERE `request_id` = ?")
                .bind(request_id)
                .fetch_one(&mut *tx)
                .await?;
        if review_count != 1 {
            return Err(WalletError::RequestConflict);
        }
        // 锁序：钱包→冻结记录。
        account::lock_wallet_account_tx(&mut tx, user_id).await?;
        let (freeze_id, freeze_user, freeze_status, trigger_rule): (u64, u64, String, String) =
            sqlx::query_as(
                "SELECT f.`id`, f.`user_id`, f.`status`, f.`trigger_rule` \
                 FROM `risk_freeze_log` f \
                 JOIN `wallet_risk_freeze_link` l ON l.`freeze_id` = f.`id` \
                 WHERE l.`request_id` = ? LIMIT 1 FOR UPDATE",
            )
            .bind(request_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(WalletError::NotFound)?;
        if freeze_user != user_id
            || trigger_rule != "wallet_refund_frequency"
            || freeze_status != "frozen"
        {
            return Err(WalletError::RequestConflict);
        }
        sqlx::query(
            "UPDATE `risk_freeze_log` SET `status` = 'unfrozen', `unfreeze_at` = UTC_TIMESTAMP(3) \
             WHERE `id` = ?",
        )
        .bind(freeze_id)
        .execute(&mut *tx)
        .await?;
        // 剩余生效冻结数与用户状态分别判定（两列取值串相同但事实不同）。
        let (active,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM `risk_freeze_log` \
             WHERE `user_id` = ? AND `status` = 'frozen' FOR UPDATE",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        let (user_status,): (String,) = sqlx::query_as(
            "SELECT `status` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
        )
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
        let restore = active == 0 && user_status == "active";
        if restore {
            // 钱包 ID 从锁内账户行取（lock_wallet_account_tx 返回后按 user 查一次主键）。
            let (wallet_id,): (u64,) = sqlx::query_as(
                "SELECT `id` FROM `wallet_account` WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
            )
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
            account::activate_wallet_tx(&mut tx, wallet_id).await?;
        }
        let response = serde_json::json!({
            "actor_id": actor.id.to_string(),
            "comment": input.comment,
            "wallet_active": restore,
        });
        // response_json 为 json 列：绑定 Value。
        let response_json = response.clone();
        sqlx::query(
            "INSERT INTO `wallet_risk_release` (`request_id`, `actor_id`, `comment`, `response_json`) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(actor.id as i64)
        .bind(&input.comment)
        .bind(&response_json)
        .execute(&mut *tx)
        .await?;
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

/// 锁定退款申请并返回用户与金额（旧版 RiskRepo.LockRefundRequest）。
async fn lock_refund_request(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<(u64, i64), WalletError> {
    sqlx::query_as(
        "SELECT `user_id`, `amount_cents` FROM `wallet_refund_request` \
         WHERE `request_id` = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(WalletError::NotFound)
}

type PriorRow = (u64, bool, String, String);

/// 读取审核记录（actor_id, approved, comment, response_json）；无记录 None。
async fn find_risk_review(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<PriorRow>, WalletError> {
    sqlx::query_as(
        "SELECT `actor_id`, `approved`, `comment`, CAST(`response_json` AS CHAR) FROM `wallet_risk_review` \
         WHERE `request_id` = ? LIMIT 1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 读取解冻记录（actor_id, comment, response_json → approved 恒 false 占位）；无记录 None。
async fn find_risk_release(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, String, String)>, WalletError> {
    sqlx::query_as(
        "SELECT `actor_id`, `comment`, CAST(`response_json` AS CHAR) FROM `wallet_risk_release` \
         WHERE `request_id` = ? LIMIT 1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 首次回执 JSON 解析；空值/坏值保留 None（旧版 riskReceipt 的展示语义）。
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
