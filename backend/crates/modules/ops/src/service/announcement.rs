//! 公告用例：后台发布（创建即 published，无草稿）与 C 端查询（仅 global 生效窗口内）。
//! SQL 在 repo/announcement.rs。

use crate::repo;
use crate::repo::announcement;
use crate::{OpsError, Profile};
use serde_json::{Value, json};
use sqlx::MySqlPool;

pub use crate::repo::announcement::{AnnouncementRow, PublicAnnouncement};

pub struct PublishInput {
    pub title: String,
    pub content: String,
    pub scope: String,
    pub target_ids: Option<Vec<String>>,
    pub start_at: String,
    pub end_at: Option<String>,
}

/// 发布（创建即 published）：校验 → 集合锁 → 操作者锁 → 权限 → 插入 + 审计（同事务）。
pub async fn publish(
    pool: &MySqlPool,
    actor: &Profile,
    input: PublishInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> Result<u64, OpsError> {
    if input.title.trim().is_empty()
        || input.title.chars().count() > 128
        || input.content.trim().is_empty()
        || input.content.chars().count() > 20_000
    {
        return Err(OpsError::bad_input("请填写标题、内容、有效时间和公告目标"));
    }
    let scope = chargepilot_domain::operations::AnnouncementScope::parse(input.scope.as_str())
        .map_err(|_| OpsError::bad_input("公告目标无效"))?;
    let start = chrono::DateTime::parse_from_rfc3339(&input.start_at)
        .ok()
        .filter(|t| *t > chrono::Utc::now() - chrono::Duration::hours(1))
        .ok_or_else(|| OpsError::bad_input("请填写标题、内容、有效时间和公告目标"))?;
    let end = match &input.end_at {
        Some(e) => {
            let parsed = chrono::DateTime::parse_from_rfc3339(e)
                .map_err(|_| OpsError::bad_input("请填写标题、内容、有效时间和公告目标"))?;
            if parsed <= start {
                return Err(OpsError::bad_input("请填写标题、内容、有效时间和公告目标"));
            }
            Some(parsed.with_timezone(&chrono::Utc))
        }
        None => None,
    };
    let targets = match (scope, &input.target_ids) {
        (chargepilot_domain::operations::AnnouncementScope::Global, _) => Some(json!([])),
        (_, None) => return Err(OpsError::bad_input("请填写标题、内容、有效时间和公告目标")),
        (_, Some(ids)) => {
            if ids.is_empty() || ids.len() > 100 {
                return Err(OpsError::bad_input("公告目标最多 100 项"));
            }
            if ids.iter().any(|i| i.is_empty() || i.chars().count() > 64) {
                return Err(OpsError::bad_input("公告目标无效"));
            }
            Some(json!(ids))
        }
    };
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == crate::perms::ANNOUNCEMENT_CREATE)
    {
        return Err(OpsError::Permission);
    }
    let id = allocator.next_id().await.map_err(OpsError::from)?;
    announcement::insert_published_tx(
        &mut tx,
        id,
        input.title.trim(),
        &input.content,
        scope,
        targets.as_ref().unwrap_or(&Value::Null),
        start.with_timezone(&chrono::Utc),
        end,
        actor.id,
    )
    .await?;
    let after = json!({"id": id, "title": input.title, "scope": input.scope, "status": "published", "created_by": actor.id});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "announcement",
        "create",
        "announcement",
        &id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(id)
}

/// 后台公告列表。
pub async fn list(pool: &MySqlPool) -> Result<Vec<AnnouncementRow>, OpsError> {
    announcement::all_rows(pool).await
}

/// C 端公告分页。
pub async fn public_list(
    pool: &MySqlPool,
    page: u32,
    page_size: u32,
) -> Result<(Vec<PublicAnnouncement>, u32, u32), OpsError> {
    let rows = announcement::public_rows(pool, page, page_size).await?;
    Ok((rows, page, page_size))
}
