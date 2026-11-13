//! 到期任务清理（旧版 cleanup.go + download.go 的 expireInTx 对应物）：
//! 每批最多 1000 个 completed/failed 候选，逐条锁读并重新核对时间和状态，
//! 清理后标为 expired；重复执行不重复计数，不接管生成中的任务。

use crate::artifact;
use crate::service::{ExportError, TASK_COLUMNS, TaskTup, task_from};
use chargepilot_domain::operations::ExportStatus;
use sqlx::{Acquire, MySqlConnection, MySqlPool};
use std::path::Path;

/// 每批清理的候选上限（旧版硬编码 1000）。
pub const CLEANUP_BATCH: i64 = 1000;

/// 任务锁内按可信目录和任务号清理文件，再标为 expired。
/// 文件不存在视为已清理，不使用数据库 FilePath 删除。
pub(crate) async fn expire_task(
    c: &mut MySqlConnection,
    export_dir: &str,
    task_no: &str,
    format: &str,
) -> Result<(), ExportError> {
    artifact::remove_artifacts(Path::new(export_dir), task_no, format)?;
    sqlx::query("UPDATE `export_task` SET `status` = ? WHERE `task_no` = ?")
        .bind(ExportStatus::EXPIRED)
        .bind(task_no)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 清理到期任务：返回首次标记数量（旧版 CleanupExpired）。
pub async fn cleanup_expired(pool: &MySqlPool, export_dir: &str) -> Result<usize, ExportError> {
    let now = chrono::Utc::now();
    let rows: Vec<(u64,)> = sqlx::query_as(
        "SELECT `id` FROM `export_task` \
         WHERE `status` IN ('completed','failed') AND `expires_at` <= ? \
         ORDER BY `id` LIMIT ?",
    )
    .bind(now)
    .bind(CLEANUP_BATCH)
    .fetch_all(pool)
    .await?;
    let mut removed = 0usize;
    for (id,) in rows {
        let mut conn = pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let c = tx.acquire().await?;
        let row_tup: Option<TaskTup> = sqlx::query_as(&format!(
            "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `id` = ? FOR UPDATE"
        ))
        .bind(id as i64)
        .fetch_optional(&mut *c)
        .await?;
        let Some(row_tup) = row_tup else {
            tx.commit().await?;
            continue;
        };
        let row = task_from(row_tup);
        if row.status != ExportStatus::COMPLETED && row.status != ExportStatus::FAILED {
            tx.commit().await?;
            continue;
        }
        let expiry: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT `expires_at` FROM `export_task` WHERE `id` = ? FOR SHARE")
                .bind(row.id as i64)
                .fetch_one(&mut *c)
                .await?;
        let expired = match expiry {
            Some(e) => chrono::Utc::now() >= e,
            None => false,
        };
        if !expired {
            tx.commit().await?;
            continue;
        }
        expire_task(&mut *c, export_dir, &row.task_no, &row.file_format).await?;
        tx.commit().await?;
        removed += 1;
    }
    Ok(removed)
}
