//! 用户充电反馈（评价/投诉/建议）：后台分页列表与回复/关闭（旧版
//! internal/order/feedback.go + feedback_repo.go 契约对齐）。
//! 列表输出全列（ENUM/JSON 列 CAST AS CHAR），id/user_id/order_id 为字符串；
//! 回复仅 pending→processed，关闭允许任意未关闭状态，重复关闭静默成功。

use crate::OrderError;
use chargepilot_domain::order::{ChargeOrderStatus, FeedbackCategory, FeedbackStatus};
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
    // 评价列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type FeedbackRow = (
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
    );
    let rows: Vec<FeedbackRow> = sqlx::query_as(
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
    let status: FeedbackStatus = sqlx::query_scalar(
        "SELECT `status` FROM `feedback` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or(FeedbackError::NotFound)?;
    if action == "close" && status == FeedbackStatus::Closed {
        tx.commit().await?;
        return Ok(());
    }
    if status == FeedbackStatus::Closed || (action == "reply" && status != FeedbackStatus::Pending)
    {
        return Err(FeedbackError::Conflict);
    }
    let next_status = if action == "reply" {
        sqlx::query(
            "UPDATE `feedback` SET `status` = ?, `reply_content` = ?, \
             `replied_by` = ?, `replied_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
        )
        .bind(FeedbackStatus::Processed)
        .bind(reply_content)
        .bind(actor_locked.id)
        .bind(id)
        .execute(&mut *c)
        .await?;
        FeedbackStatus::Processed
    } else {
        sqlx::query("UPDATE `feedback` SET `status` = ? WHERE `id` = ?")
            .bind(FeedbackStatus::Closed)
            .bind(id)
            .execute(&mut *c)
            .await?;
        FeedbackStatus::Closed
    };
    let after = if action == "reply" {
        json!({
            "status": next_status,
            "reply_content": reply_content,
            "replied_by": actor_locked.id,
            "replied_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    } else {
        json!({"status": next_status})
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

// ---------------------------------------------------------------------------
// 用户端提交（旧版 internal/order/feedback.go 的 ResolveReference /
// ValidateFeedback / Submit + feedback_repo.go 的 ResolveOwnedReference /
// LockOrder / HasFeedback / Insert 契约对齐）
// ---------------------------------------------------------------------------

/// 用户提交反馈错误（HTTP 映射沿用旧版字面量契约）。
#[derive(Debug, thiserror::Error)]
pub enum FeedbackSubmitError {
    /// 订单引用或反馈字段校验失败（400/1005，文案透出）。
    #[error("{0}")]
    Input(String),
    /// 订单缺失、属于他人或状态不可评价（404/1004 订单不存在或不可评价）。
    #[error("order is not a completed order of this user")]
    OrderNotReady,
    /// 订单已评价，重复提交为冲突（409/2009 该订单已评价）。
    #[error("order already has feedback")]
    Exists,
    /// 按业务单号解析时订单不存在（404/1004 订单不存在）。
    #[error("order not found")]
    NotFound,
    /// 存储错误（503/5003 反馈提交失败，请稍后重试）。
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for FeedbackSubmitError {
    fn from(e: sqlx::Error) -> Self {
        FeedbackSubmitError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

impl From<serde_json::Error> for FeedbackSubmitError {
    fn from(e: serde_json::Error) -> Self {
        FeedbackSubmitError::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 用户反馈提交字段（旧版 FeedbackInput）。
/// `images` 为 None 时保存 JSON null（旧版 json.Marshal(nil slice)）；
/// 空数组保存 `[]`；归属由事务中的订单提供。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FeedbackSubmitInput {
    /// 1–5 评分。
    pub rating: i8,
    /// 反馈分类：rating / complaint / suggestion。
    pub category: Option<String>,
    /// 可空正文，最多 2000 个字符。
    pub content: Option<String>,
    /// 附件图片，最多 5 个 HTTPS 链接；缺省保存 JSON null。
    #[serde(default)]
    pub images: Option<Vec<String>>,
}

/// 将路径参数解析为订单主键（旧版 ResolveReference）。
/// 纯数字（>0，含前导零）直接当主键，归属留待提交事务核对；其余按业务单号
/// 解析：去除首尾空白后为空或超 64 字节按输入错误拒绝，查询限定本人未删除订单。
pub async fn resolve_reference(
    pool: &MySqlPool,
    user_id: u64,
    raw: &str,
) -> Result<u64, FeedbackSubmitError> {
    if let Ok(id) = raw.parse::<u64>()
        && id > 0
    {
        return Ok(id);
    }
    let order_no = raw.trim();
    if order_no.is_empty() || order_no.len() > 64 {
        return Err(FeedbackSubmitError::Input("订单号无效".into()));
    }
    let id: Option<u64> = sqlx::query_scalar(
        "SELECT `id` FROM `charge_order` \
         WHERE `order_no` = ? AND `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(order_no)
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await?;
    id.ok_or(FeedbackSubmitError::NotFound)
}

/// 校验评分、类别、内容长度与附件（旧版 ValidateFeedback；文案逐字对齐）。
pub fn validate_feedback(input: &FeedbackSubmitInput) -> Result<(), FeedbackSubmitError> {
    if !(1..=5).contains(&input.rating) {
        return Err(FeedbackSubmitError::Input("评分或反馈类型无效".into()));
    }
    if input
        .category
        .as_deref()
        .is_some_and(|c| FeedbackCategory::parse(c).is_err())
    {
        return Err(FeedbackSubmitError::Input("评分或反馈类型无效".into()));
    }
    if let Some(content) = &input.content
        && content.chars().count() > 2000
    {
        return Err(FeedbackSubmitError::Input("反馈内容过长".into()));
    }
    let images = input.images.as_deref().unwrap_or(&[]);
    if images.len() > 5 {
        return Err(FeedbackSubmitError::Input("图片最多 5 张".into()));
    }
    for image in images {
        if !valid_feedback_image(image) {
            return Err(FeedbackSubmitError::Input("图片链接必须为 HTTPS".into()));
        }
    }
    Ok(())
}

/// 核对附件为无首尾空白、认证信息的 HTTPS 链接，最多 512 字节
/// （旧版 ValidFeedbackImage：url.Parse 后 scheme=https、Host 非空、User 为 nil）。
/// 不引入 url 依赖，按同等语义手写：scheme 大小写不敏感，authority 段
/// 非空、不含 userinfo 与空白。
fn valid_feedback_image(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 512 || raw.trim() != raw {
        return false;
    }
    // 控制字符在任何段都不合法（旧版 url.Parse 直接报错）。
    if raw.bytes().any(|b| b <= 0x1F || b == 0x7F) {
        return false;
    }
    let lowered = raw.to_ascii_lowercase();
    let Some(rest) = lowered.strip_prefix("https://") else {
        return false;
    };
    // authority 段：首个 '/' 之前；Host 非空、无 userinfo、无空白。
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    !authority.is_empty() && !authority.contains('@') && !authority.contains([' ', '\t'])
}

/// 为已完成且属于当前用户的订单保存反馈（旧版 FeedbackService.Submit）。
/// 事务顺序：锁订单行 → 核对归属与状态（completed/refunding/refunded）
/// → SHARE 锁核对已有反馈（防止旧快照漏掉并发提交）→ 插入 pending 反馈。
/// 重复反馈报冲突，任一步失败整体回滚；feedback_id 为新反馈自增主键。
pub async fn submit_feedback(
    pool: &MySqlPool,
    user_id: u64,
    order_id: u64,
    input: &FeedbackSubmitInput,
) -> Result<u64, FeedbackSubmitError> {
    validate_feedback(input)?;
    // 校验已保证 category 为合法词表值；parse 后按枚举绑定写入。
    let category = match input.category.as_deref().map(FeedbackCategory::parse) {
        Some(Ok(c)) => c,
        _ => return Err(FeedbackSubmitError::Input("评分或反馈类型无效".into())),
    };
    // nil 保存 JSON null，空数组保存 []（旧版 json.Marshal 语义）。
    let images_json = match &input.images {
        Some(v) => serde_json::to_string(v)?,
        None => "null".to_string(),
    };
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    // 锁订单（旧版 LockOrder FOR UPDATE）：缺失、他人或不可评价状态统一拒绝。
    let order: Option<(u64, Option<String>, ChargeOrderStatus)> = sqlx::query_as(
        "SELECT `user_id`, `device_id`, `status` \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner, device_id, status)) = order else {
        return Err(FeedbackSubmitError::OrderNotReady);
    };
    if owner != user_id
        || !matches!(
            status,
            ChargeOrderStatus::Completed
                | ChargeOrderStatus::Refunding
                | ChargeOrderStatus::Refunded
        )
    {
        return Err(FeedbackSubmitError::OrderNotReady);
    }
    // 当前读核对已有反馈（旧版 HasFeedback FOR SHARE）。
    let existing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `feedback` \
         WHERE `order_id` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(order_id as i64)
    .fetch_one(&mut *tx)
    .await?;
    if existing > 0 {
        return Err(FeedbackSubmitError::Exists);
    }
    // 插入初始 pending 反馈；请求身份来自已锁订单（旧版 Insert）。
    let id = sqlx::query(
        "INSERT INTO `feedback` \
         (`user_id`, `order_id`, `device_id`, `rating`, `category`, `content`, `images_json`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(owner as i64)
    .bind(order_id as i64)
    .bind(device_id)
    .bind(input.rating)
    .bind(category)
    .bind(&input.content)
    .bind(&images_json)
    .execute(&mut *tx)
    .await?
    .last_insert_id() as u64;
    tx.commit().await?;
    Ok(id)
}
