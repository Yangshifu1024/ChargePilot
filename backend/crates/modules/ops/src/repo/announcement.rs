//! announcement 表访问（蓝图第五章 repo 层）。

use crate::OpsError;
use chrono::{DateTime, Utc};
use sqlx::{MySqlConnection, MySqlPool};

/// 后台公告行。
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

/// C 端公告行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct PublicAnnouncement {
    pub id: u64,
    pub title: String,
    pub content: String,
    pub start_at: String,
    pub end_at: Option<String>,
}

/// 后台公告全量行（id 倒序）。
pub async fn all_rows(pool: &MySqlPool) -> Result<Vec<AnnouncementRow>, OpsError> {
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

/// 插入 published 公告（校验与锁序由调用方完成）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_published_tx(
    tx: &mut MySqlConnection,
    id: u64,
    title: &str,
    content: &str,
    scope: chargepilot_domain::operations::AnnouncementScope,
    target_ids: &serde_json::Value,
    start_at: DateTime<Utc>,
    end_at: Option<DateTime<Utc>>,
    created_by: u64,
) -> Result<(), OpsError> {
    sqlx::query(
        "INSERT INTO `announcement` (`id`, `title`, `content`, `scope`, `target_ids`, `status`, `start_at`, `end_at`, `created_by`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(title)
    .bind(content)
    .bind(scope)
    .bind(target_ids)
    .bind(chargepilot_domain::operations::AnnouncementStatus::Published)
    .bind(start_at)
    .bind(end_at)
    .bind(created_by)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// C 端公告分页：仅 published + 生效窗口内 + scope=global。
pub async fn public_rows(
    pool: &MySqlPool,
    page: u32,
    page_size: u32,
) -> Result<Vec<PublicAnnouncement>, OpsError> {
    sqlx::query_as::<_, PublicAnnouncement>(
        "SELECT `id`, `title`, `content`, \
                DATE_FORMAT(`start_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `start_at`, \
                DATE_FORMAT(`end_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `end_at` \
         FROM `announcement` \
         WHERE `deleted_at` IS NULL AND `status` = ? AND `scope` = ? \
           AND `start_at` <= NOW(3) AND (`end_at` IS NULL OR `end_at` >= NOW(3)) \
         ORDER BY `start_at` DESC, `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(chargepilot_domain::operations::AnnouncementStatus::Published)
    .bind(chargepilot_domain::operations::AnnouncementScope::Global)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await
    .map_err(OpsError::from)
}
