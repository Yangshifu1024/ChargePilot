//! 风控链表访问：wallet_refund_request / wallet_risk_review / wallet_risk_release /
//! risk_freeze_log / wallet_risk_freeze_link（蓝图第五章 repo 层）。

use crate::WalletError;
use sqlx::{MySqlConnection, MySqlPool};

/// 列表原始行（review/release 的 response_json 以字符串抓取后由调用方解析）。
#[derive(Debug, sqlx::FromRow)]
pub struct WalletRiskRawRow {
    pub request_id: String,
    pub user_id: u64,
    pub amount_cents: i64,
    pub reason: Option<String>,
    pub created_at: Option<String>,
    pub review_json: Option<String>,
    pub review_created_at: Option<String>,
    pub release_json: Option<String>,
    pub release_created_at: Option<String>,
    pub freeze_status: Option<String>,
    pub freeze_linked: i64,
}

/// 风控申请列表行（reviewed 过滤在 SQL 内完成）。
pub async fn risk_rows(
    pool: &MySqlPool,
    reviewed: bool,
    size: u32,
    offset: u32,
) -> Result<Vec<WalletRiskRawRow>, WalletError> {
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
    Ok(sqlx::query_as::<_, WalletRiskRawRow>(&list_sql)
        .bind(size.max(1))
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

/// 风控申请计数（与列表同过滤）。
pub async fn risk_count(pool: &MySqlPool, reviewed: bool) -> Result<i64, WalletError> {
    let null_clause = if reviewed { "IS NOT NULL" } else { "IS NULL" };
    let count_sql = format!(
        "SELECT COUNT(*) FROM `wallet_refund_request` r \
         LEFT JOIN `wallet_risk_review` v ON v.`request_id` = r.`request_id` \
         JOIN `wallet_risk_freeze_link` l ON l.`request_id` = r.`request_id` \
         WHERE v.`request_id` {null_clause}"
    );
    let (total,): (i64,) = sqlx::query_as(&count_sql).fetch_one(pool).await?;
    Ok(total)
}

/// 锁定退款申请并返回用户与金额。
pub async fn lock_refund_request(
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

pub type PriorRow = (u64, bool, String, String);

/// 读取审核记录（actor_id, approved, comment, response_json）；无记录 None。
pub async fn find_risk_review(
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

/// 读取解冻记录（actor_id, comment, response_json）；无记录 None。
pub async fn find_risk_release(
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

/// 必须恰好关联一条 frequency 冻结：返回关联计数。
pub async fn frequency_freeze_count(
    tx: &mut MySqlConnection,
    request_id: &str,
    user_id: u64,
) -> Result<i64, WalletError> {
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
    Ok(linked)
}

/// 保存审核回执（response_json 为 json 列：绑定 Value）。
pub async fn insert_review_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    actor_id: u64,
    approved: bool,
    comment: &str,
    response_json: &serde_json::Value,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_risk_review` (`request_id`, `actor_id`, `approved`, \
         `comment`, `response_json`) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(actor_id as i64)
    .bind(approved)
    .bind(comment)
    .bind(response_json)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 审核须恰好一条：返回审核计数。
pub async fn review_count(tx: &mut MySqlConnection, request_id: &str) -> Result<i64, WalletError> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM `wallet_risk_review` WHERE `request_id` = ?")
            .bind(request_id)
            .fetch_one(&mut *tx)
            .await?;
    Ok(n)
}

/// 锁读冻结记录（钱包锁后；锁序：钱包→冻结记录）。
pub async fn lock_freeze_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, u64, String, String)>, WalletError> {
    sqlx::query_as(
        "SELECT f.`id`, f.`user_id`, f.`status`, f.`trigger_rule` \
         FROM `risk_freeze_log` f \
         JOIN `wallet_risk_freeze_link` l ON l.`freeze_id` = f.`id` \
         WHERE l.`request_id` = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Into::into)
}

/// 解冻落库（unfreeze_at 推进）。
pub async fn unfreeze_tx(
    tx: &mut MySqlConnection,
    freeze_id: u64,
    unfrozen: chargepilot_domain::wallet::FreezeStatus,
) -> Result<(), WalletError> {
    sqlx::query("UPDATE `risk_freeze_log` SET `status` = ?, `unfreeze_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(unfrozen)
        .bind(freeze_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 剩余生效冻结数（FOR UPDATE）。
pub async fn active_freeze_count(
    tx: &mut MySqlConnection,
    user_id: u64,
    frozen: chargepilot_domain::wallet::FreezeStatus,
) -> Result<i64, WalletError> {
    let (active,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM `risk_freeze_log` \
         WHERE `user_id` = ? AND `status` = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(frozen)
    .fetch_one(&mut *tx)
    .await?;
    Ok(active)
}

/// 用户启用状态（解冻恢复判定第二事实）。
pub async fn user_status_active(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<bool, WalletError> {
    let (user_status,): (chargepilot_domain::identity::UserStatus,) = sqlx::query_as(
        "SELECT `status` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(user_status == chargepilot_domain::identity::UserStatus::Active)
}

/// 钱包主键（锁内账户行取回后按 user 查一次）。
pub async fn wallet_id_by_user_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
) -> Result<u64, WalletError> {
    let (wallet_id,): (u64,) = sqlx::query_as(
        "SELECT `id` FROM `wallet_account` WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(wallet_id)
}

/// 保存解冻回执（response_json 为 json 列）。
pub async fn insert_release_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    actor_id: u64,
    comment: &str,
    response_json: &serde_json::Value,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO `wallet_risk_release` (`request_id`, `actor_id`, `comment`, `response_json`) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(actor_id as i64)
    .bind(comment)
    .bind(response_json)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
