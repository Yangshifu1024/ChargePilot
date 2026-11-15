//! 用户充电反馈（评价/投诉/建议）：后台回复/关闭与用户端提交的用例编排。
//! 列表读侧在 repo/feedback.rs；回复仅 pending→processed，关闭允许任意
//! 未关闭状态，重复关闭静默成功。

use crate::error::{FeedbackError, FeedbackSubmitError};
use crate::repo::feedback;
// 后台列表读侧留在 repo（蓝图审计：service 层无 SQL）；这里 re-export
// 保持 `chargepilot_mod_order::feedback::list` 既有路径不变。
pub use crate::repo::feedback::list;
use chargepilot_domain::order::{ChargeOrderStatus, FeedbackCategory, FeedbackStatus};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo as ops_repo;
use serde_json::json;
use sqlx::{Acquire, MySqlPool};

/// 回复或关闭反馈：反馈工单属全局管理功能，不按站点过滤；
/// 锁内核对实时 feedback.reply 权限；状态与审计同事务。
/// 回复仅允许 pending→processed；关闭允许任意未关闭状态，重复关闭静默成功。
/// 审计 module=feedback、action=reply/close、target_type=feedback。
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
    let mut conn = pool.acquire().await?;
    feedback::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let actor_locked = ops_repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(|_| FeedbackError::Forbidden)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "feedback.reply")
    {
        return Err(FeedbackError::Forbidden);
    }
    let status = feedback::lock_feedback_tx(&mut tx, id)
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
        feedback::update_reply_tx(&mut tx, id, reply_content, actor_locked.id).await?;
        FeedbackStatus::Processed
    } else {
        feedback::update_status_tx(&mut tx, id).await?;
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
    ops_repo::append_audit_tx(
        &mut tx,
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
// 用户端提交：归属解析 / 校验 / 提交事务（SQL 原语在 repo/feedback.rs）
// ---------------------------------------------------------------------------

/// 用户反馈提交字段。
/// `images` 为 None 时保存 JSON null；空数组保存 `[]`；归属由事务中的订单提供。
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

/// 将路径参数解析为订单主键。
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
    feedback::find_owned_order_id(pool, order_no, user_id)
        .await?
        .ok_or(FeedbackSubmitError::NotFound)
}

/// 校验评分、类别、内容长度与附件（文案逐字对齐现有契约）。
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
/// （scheme=https、authority 非空、不含 userinfo 与空白）。
/// 不引入 url 依赖，按同等语义手写。
fn valid_feedback_image(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 512 || raw.trim() != raw {
        return false;
    }
    // 控制字符在任何段都不合法。
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

/// 为已完成且属于当前用户的订单保存反馈。
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
    // nil 保存 JSON null，空数组保存 []。
    let images_json = match &input.images {
        Some(v) => serde_json::to_string(v)?,
        None => "null".to_string(),
    };
    let mut conn = pool.acquire().await?;
    feedback::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    // 锁订单：缺失、他人或不可评价状态统一拒绝。
    let Some((owner, device_id, status)) = feedback::lock_order_tx(&mut tx, order_id).await? else {
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
    // 当前读核对已有反馈。
    let existing = feedback::feedback_count_share_tx(&mut tx, order_id).await?;
    if existing > 0 {
        return Err(FeedbackSubmitError::Exists);
    }
    // 插入初始 pending 反馈；请求身份来自已锁订单。
    let id = feedback::insert_feedback_tx(
        &mut tx,
        owner,
        order_id,
        device_id,
        input.rating,
        category,
        input.content.as_deref(),
        &images_json,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
