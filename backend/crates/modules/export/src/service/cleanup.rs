//! 到期任务清理用例：每批最多 1000 个 completed/failed 候选，逐条锁读并
//! 重新核对时间和状态，清理后标为 expired；重复执行不重复计数，
//! 不接管生成中的任务。SQL 原语在 repo/tasks.rs。

use crate::artifact;
use crate::error::ExportError;
use crate::repo::tasks;
use sqlx::{Acquire, MySqlConnection, MySqlPool};
use std::path::Path;

/// 每批清理的候选上限。
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
    tasks::mark_expired_tx(c, task_no).await?;
    Ok(())
}

/// 清理到期任务：返回首次标记数量。
pub async fn cleanup_expired(pool: &MySqlPool, export_dir: &str) -> Result<usize, ExportError> {
    let now = chrono::Utc::now();
    let rows = tasks::expired_candidates(pool, now, CLEANUP_BATCH).await?;
    let mut removed = 0usize;
    for (id,) in rows {
        let mut conn = pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let c = tx.acquire().await?;
        let row_tup = tasks::lock_task_by_id_tx(&mut *c, id).await?;
        let Some(row_tup) = row_tup else {
            tx.commit().await?;
            continue;
        };
        let row = tasks::task_from(row_tup);
        use chargepilot_domain::export::ExportStatus;
        if row.status != ExportStatus::Completed && row.status != ExportStatus::Failed {
            tx.commit().await?;
            continue;
        }
        let expiry = tasks::expiry_for_share_tx(&mut *c, row.id).await?;
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
