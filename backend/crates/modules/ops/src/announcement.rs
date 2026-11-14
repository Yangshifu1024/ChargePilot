//! 公告：后台发布（创建即 published，无草稿）与 C 端查询（仅 global 生效窗口内）。

use super::repo;
use super::{OpsError, Profile};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct AnnouncementRow {
    pub id: u64,
    pub title: String,
    pub content: String,
    pub scope: String,
    pub target_ids: Option<serde_json::Value>,
    pub status: String,
    pub start_at: String,
    pub end_at: Option<String>,
}

pub async fn list(pool: &MySqlPool) -> Result<Vec<AnnouncementRow>, OpsError> {
    sqlx::query_as::<_, AnnouncementRow>(
        "SELECT `id`, `title`, `content`, `scope`, `target_ids`, `status`, \
                DATE_FORMAT(`start_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `start_at`, \
                DATE_FORMAT(`end_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `end_at` \
         FROM `announcement` WHERE `deleted_at` IS NULL ORDER BY `id` DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(OpsError::from)
}

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
    if !matches!(input.scope.as_str(), "global" | "station" | "city") {
        return Err(OpsError::bad_input("公告目标无效"));
    }
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
    let targets = match (&input.scope[..], &input.target_ids) {
        ("global", _) => Some(json!([])),
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
        .any(|p| p == "announcement.create")
    {
        return Err(OpsError::Permission);
    }
    let id = allocator.next_id().await.map_err(OpsError::from)?;
    sqlx::query(
        "INSERT INTO `announcement` (`id`, `title`, `content`, `scope`, `target_ids`, `status`, `start_at`, `end_at`, `created_by`) \
         VALUES (?, ?, ?, ?, ?, 'published', ?, ?, ?)",
    )
    .bind(id)
    .bind(input.title.trim())
    .bind(&input.content)
    .bind(&input.scope)
    .bind(&targets)
    .bind(start.with_timezone(&chrono::Utc))
    .bind(end)
    .bind(actor.id)
    .execute(&mut *tx)
    .await
    .map_err(OpsError::from)?;
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

/// C 端公告分页：仅 published + 生效窗口内 + scope=global。
/// 未提供位置上下文，station/city 定向公告不进入全局列表（旧版
/// AnnouncementQueryService.List 以 stationID=0 内存过滤，此处改为 SQL 过滤）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct PublicAnnouncement {
    pub id: u64,
    pub title: String,
    pub content: String,
    pub start_at: String,
    pub end_at: Option<String>,
}

pub async fn public_list(
    pool: &MySqlPool,
    page: u32,
    page_size: u32,
) -> Result<(Vec<PublicAnnouncement>, u32, u32), OpsError> {
    let rows = sqlx::query_as::<_, PublicAnnouncement>(
        "SELECT `id`, `title`, `content`, \
                DATE_FORMAT(`start_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `start_at`, \
                DATE_FORMAT(`end_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `end_at` \
         FROM `announcement` \
         WHERE `deleted_at` IS NULL AND `status` = 'published' AND `scope` = 'global' \
           AND `start_at` <= NOW(3) AND (`end_at` IS NULL OR `end_at` >= NOW(3)) \
         ORDER BY `start_at` DESC, `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await
    .map_err(OpsError::from)?;
    Ok((rows, page, page_size))
}
