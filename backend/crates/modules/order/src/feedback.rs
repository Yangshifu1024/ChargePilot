//! 用户充电反馈（评价/投诉/建议）：后台分页列表与回复/关闭（旧版
//! internal/order/feedback.go + feedback_repo.go 契约对齐）。
//! 列表输出全列（ENUM/JSON 列 CAST AS CHAR），id/user_id/order_id 为字符串；
//! 回复仅 pending→processed，关闭允许任意未关闭状态，重复关闭静默成功。

use crate::OrderError;
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde_json::{Value, json};
use sqlx::Acquire;
use sqlx::MySqlPool;

/// 反馈业务错误（HTTP 层映射：Input→400、Conflict→409、NotFound→404、Forbidden→403）。
#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    /// 业务输入错误（文案透出）。
    #[error("{0}")]
    Input(String),
    /// 状态流转冲突（已关闭或非 pending 回复）。
    #[error("反馈状态已变化，请刷新后重试")]
    Conflict,
    /// 反馈不存在或已删除。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 实时权限不足。
    #[error("没有操作权限")]
    Forbidden,
    /// 存储错误。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for FeedbackError {
    fn from(e: sqlx::Error) -> Self {
        FeedbackError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

impl From<OrderError> for FeedbackError {
    fn from(e: OrderError) -> Self {
        FeedbackError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 后台分页列表：状态筛选（空串不筛选）+ 已校验分页，返回行与总数（旧版 FeedbackRepo.List）。
/// 输出列与旧版 SELECT * 对齐；id/user_id/order_id 转字符串（旧版 DecimalizeRowIDsAll），
/// images_json 解析为数组改名 images（缺省 []），ENUM/JSON 列 CAST AS CHAR。
pub async fn list(
    pool: &MySqlPool,
    status: &str,
    page: u32,
    page_size: u32,
) -> Result<(Vec<Value>, i64), FeedbackError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `feedback` WHERE `deleted_at` IS NULL AND (? = '' OR `status` = ?)",
    )
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    let rows: Vec<(
        u64,
        u64,
        Option<u64>,
        Option<String>,
        Option<u8>,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<u64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT `id`, `user_id`, `order_id`, `device_id`, `rating`, \
                CAST(`category` AS CHAR) AS `category`, `content`, \
                CAST(`images_json` AS CHAR) AS `images_json`, \
                CAST(`status` AS CHAR) AS `status`, `replied_by`, \
                DATE_FORMAT(`replied_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `replied_at`, \
                `reply_content`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                DATE_FORMAT(`deleted_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `deleted_at` \
         FROM `feedback` WHERE `deleted_at` IS NULL AND (? = '' OR `status` = ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(status)
    .bind(status)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(
            |(
                id,
                user_id,
                order_id,
                device_id,
                rating,
                category,
                content,
                images_json,
                status,
                replied_by,
                replied_at,
                reply_content,
                created_at,
                deleted_at,
            )| {
                let images = images_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or_else(|| json!([]));
                json!({
                    "id": id.to_string(),
                    "user_id": user_id.to_string(),
                    "order_id": order_id.map(|v| v.to_string()),
                    "device_id": device_id,
                    "rating": rating,
                    "category": category,
                    "content": content,
                    "images": images,
                    "status": status,
                    "replied_by": replied_by,
                    "replied_at": replied_at,
                    "reply_content": reply_content,
                    "created_at": created_at,
                    "deleted_at": deleted_at,
                })
            },
        )
        .collect();
    Ok((items, total))
}

/// 回复或关闭反馈（旧版 FeedbackService.Reply）：反馈工单属全局管理功能，
/// 不按站点过滤；锁内核对实时 feedback.reply 权限；状态与审计同事务。
/// 回复仅允许 pending→processed；关闭允许任意未关闭状态，重复关闭静默成功。
/// 审计 module=feedback、action=reply/close、target_type=feedback（旧版 AppendAuditInTx 语义）。
pub async fn reply(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
    action: &str,
    reply_content: &str,
    ip: &str,
    request_id: &str,
) -> Result<(), FeedbackError> {
    if action != "close"
        && (action != "reply"
            || reply_content.trim().is_empty()
            || reply_content.chars().count() > 2000)
    {
        return Err(FeedbackError::Input(
            "请填写 1–2000 字回复或关闭反馈".into(),
        ));
    }
    // 写事务：READ COMMITTED + 连接级事务（旧版 gorm Transaction 语义）。
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let actor_locked = repo::lock_actor_tx(&mut *c, actor.id)
        .await
        .map_err(|_| FeedbackError::Forbidden)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "feedback.reply")
    {
        return Err(FeedbackError::Forbidden);
    }
    let status: String = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `feedback` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or(FeedbackError::NotFound)?;
    if action == "close" && status == "closed" {
        tx.commit().await?;
        return Ok(());
    }
    if status == "closed" || (action == "reply" && status != "pending") {
        return Err(FeedbackError::Conflict);
    }
    if action == "reply" {
        sqlx::query(
            "UPDATE `feedback` SET `status` = 'processed', `reply_content` = ?, \
             `replied_by` = ?, `replied_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
        )
        .bind(reply_content)
        .bind(actor_locked.id)
        .bind(id)
        .execute(&mut *c)
        .await?;
    } else {
        sqlx::query("UPDATE `feedback` SET `status` = 'closed' WHERE `id` = ?")
            .bind(id)
            .execute(&mut *c)
            .await?;
    }
    let after = if action == "reply" {
        json!({
            "status": "processed",
            "reply_content": reply_content,
            "replied_by": actor_locked.id,
            "replied_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    } else {
        json!({"status": "closed"})
    };
    repo::append_audit_tx(
        &mut *c,
        &actor_locked,
        "feedback",
        action,
        "feedback",
        &id.to_string(),
        &json!({"status": status}),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
