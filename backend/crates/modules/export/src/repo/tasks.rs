//! export_task 表访问与行投影（蓝图第五章 repo 层）：SQL 只出现在本目录。

use crate::error::ExportError;
use chargepilot_domain::export::ExportStatus;
use chrono::{DateTime, Utc};
use sqlx::{MySqlConnection, MySqlPool};

/// 任务行投影元组（列序即 [`TASK_COLUMNS`] SELECT 顺序）。
pub(crate) type TaskTup = (
    u64,
    String,
    String,
    String,
    ExportStatus,
    u64,
    Option<String>,
    u32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// 全列 SELECT 片段（ENUM/JSON/时间列 CAST AS CHAR 对齐契约输出）。
pub(crate) const TASK_COLUMNS: &str = "`id`, `task_no`, `resource`, `file_format`, `status`, `requested_by`, \
     CAST(`filter_json` AS CHAR) AS `filter_json`, `row_count`, `error_msg`, \
     CAST(`expires_at` AS CHAR) AS `expires_at`, CAST(`created_at` AS CHAR) AS `created_at`, \
     CAST(`completed_at` AS CHAR) AS `completed_at`";

/// 元组 → 对外任务行。
pub(crate) fn task_from(t: TaskTup) -> crate::service::tasks::ExportTaskRow {
    let (
        id,
        task_no,
        resource,
        file_format,
        status,
        requested_by,
        filter_json,
        row_count,
        error_msg,
        expires_at,
        created_at,
        completed_at,
    ) = t;
    crate::service::tasks::ExportTaskRow {
        id,
        task_no,
        resource,
        file_format,
        status,
        requested_by,
        filter_json,
        row_count,
        error_msg,
        expires_at,
        created_at,
        completed_at,
    }
}

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}

/// 任务总数（status None 不过滤）。
pub async fn task_count_tx(
    c: &mut MySqlConnection,
    status: Option<&str>,
) -> Result<i64, ExportError> {
    let sql = if status.is_none() {
        "SELECT COUNT(*) FROM `export_task`".to_string()
    } else {
        "SELECT COUNT(*) FROM `export_task` WHERE `status` = ?".to_string()
    };
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    if let Some(s) = status {
        q = q.bind(s);
    }
    Ok(q.fetch_one(c).await?)
}

/// 任务分页行（status None 不过滤，id 倒序）。
pub async fn list_task_tups_tx(
    c: &mut MySqlConnection,
    status: Option<&str>,
    page: i64,
    page_size: i64,
) -> Result<Vec<TaskTup>, ExportError> {
    let sql = if status.is_none() {
        format!("SELECT {TASK_COLUMNS} FROM `export_task` ORDER BY `id` DESC LIMIT ? OFFSET ?")
    } else {
        format!(
            "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `status` = ? \
             ORDER BY `id` DESC LIMIT ? OFFSET ?"
        )
    };
    let mut q = sqlx::query_as::<_, TaskTup>(&sql);
    if let Some(s) = status {
        q = q.bind(s);
    }
    q = q.bind(page_size).bind((page - 1) * page_size);
    Ok(q.fetch_all(c).await?)
}

/// 按主键读任务行（无锁）。
pub async fn task_by_id_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<TaskTup>, ExportError> {
    sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `id` = ?"
    ))
    .bind(id as i64)
    .fetch_optional(c)
    .await
    .map_err(ExportError::from)
}

/// 按主键锁读任务行（FOR UPDATE）。
pub async fn lock_task_by_id_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<TaskTup>, ExportError> {
    sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `id` = ? FOR UPDATE"
    ))
    .bind(id as i64)
    .fetch_optional(c)
    .await
    .map_err(ExportError::from)
}

/// 按任务号锁读任务行（FOR UPDATE）。
pub async fn lock_task_by_no_tx(
    c: &mut MySqlConnection,
    task_no: &str,
) -> Result<Option<TaskTup>, ExportError> {
    sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ? FOR UPDATE"
    ))
    .bind(task_no)
    .fetch_optional(c)
    .await
    .map_err(ExportError::from)
}

/// 按任务号读任务行（无锁）。
pub async fn task_by_no(pool: &MySqlPool, task_no: &str) -> Result<Option<TaskTup>, ExportError> {
    sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ?"
    ))
    .bind(task_no)
    .fetch_optional(pool)
    .await
    .map_err(ExportError::from)
}

/// 预检任务号是否已存在（唯一键兜底并发登记的前置）。
pub async fn first_seen_task_id_tx(
    c: &mut MySqlConnection,
    task_no: &str,
) -> Result<Option<u64>, ExportError> {
    sqlx::query_scalar("SELECT `id` FROM `export_task` WHERE `task_no` = ?")
        .bind(task_no)
        .fetch_optional(c)
        .await
        .map_err(ExportError::from)
}

/// 登记导出任务（pending）；撞 uk_task_no 由调用方按 23000 分类。
#[allow(clippy::too_many_arguments)]
pub async fn insert_task_tx(
    c: &mut MySqlConnection,
    task_no: &str,
    resource: &str,
    file_format: &str,
    requested_by: u64,
    filter_json: &str,
    expires_at: DateTime<Utc>,
) -> Result<sqlx::mysql::MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO `export_task` (`task_no`, `resource`, `file_format`, `status`, \
         `requested_by`, `filter_json`, `expires_at`) VALUES (?, ?, ?, 'pending', ?, ?, ?)",
    )
    .bind(task_no)
    .bind(resource)
    .bind(file_format)
    .bind(requested_by as i64)
    .bind(filter_json)
    .bind(expires_at)
    .execute(c)
    .await
}

/// FOR SHARE 读有效期（锁内到期核对）。
pub async fn expiry_for_share_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<DateTime<Utc>>, ExportError> {
    sqlx::query_scalar("SELECT `expires_at` FROM `export_task` WHERE `id` = ? FOR SHARE")
        .bind(id as i64)
        .fetch_one(c)
        .await
        .map_err(ExportError::from)
}

/// 完成：completed + 计数 + 文件路径，仅 pending/running 可推进；返回推进行数。
pub async fn complete_task_tx(
    c: &mut MySqlConnection,
    task_no: &str,
    row_count: u32,
    file_path: &str,
    completed_at: DateTime<Utc>,
) -> Result<u64, ExportError> {
    Ok(sqlx::query(
        "UPDATE `export_task` SET `status` = ?, `row_count` = ?, `file_path` = ?, \
         `completed_at` = ?, `error_msg` = NULL \
         WHERE `task_no` = ? AND `status` IN ('pending','running')",
    )
    .bind(ExportStatus::Completed)
    .bind(row_count)
    .bind(file_path)
    .bind(completed_at)
    .bind(task_no)
    .execute(c)
    .await?
    .rows_affected())
}

/// 标记失败：仅 pending/running 可推进；返回推进行数。
pub async fn fail_task_tx(
    c: &mut MySqlConnection,
    task_no: &str,
    reason: &str,
) -> Result<u64, ExportError> {
    Ok(sqlx::query(
        "UPDATE `export_task` SET `status` = ?, `error_msg` = ? \
         WHERE `task_no` = ? AND `status` IN ('pending','running')",
    )
    .bind(ExportStatus::Failed)
    .bind(reason)
    .bind(task_no)
    .execute(c)
    .await?
    .rows_affected())
}

/// 锁内读服务器文件路径（只内部比对，不出网）。
pub async fn file_path_for_update_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<String>, ExportError> {
    sqlx::query_scalar("SELECT `file_path` FROM `export_task` WHERE `id` = ? FOR UPDATE")
        .bind(id as i64)
        .fetch_one(c)
        .await
        .map_err(ExportError::from)
}

/// 到期候选主键（completed/failed，按 expires_at 递增，每批上限）。
pub async fn expired_candidates(
    pool: &MySqlPool,
    now: DateTime<Utc>,
    batch: i64,
) -> Result<Vec<(u64,)>, ExportError> {
    sqlx::query_as(
        "SELECT `id` FROM `export_task` \
         WHERE `status` IN ('completed','failed') AND `expires_at` <= ? \
         ORDER BY `id` LIMIT ?",
    )
    .bind(now)
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(ExportError::from)
}

/// 清理标记：expired（按任务号幂等）。
pub async fn mark_expired_tx(c: &mut MySqlConnection, task_no: &str) -> Result<(), ExportError> {
    sqlx::query("UPDATE `export_task` SET `status` = ? WHERE `task_no` = ?")
        .bind(ExportStatus::Expired)
        .bind(task_no)
        .execute(c)
        .await?;
    Ok(())
}

/// 按登记资源读取业务数据：依次过滤软删除、账单日期与数据范围；
/// limit 为上限加一，多出的一行用于判断超限。
pub async fn query_export_source(
    c: &mut MySqlConnection,
    spec: &crate::registry::ExportSpec,
    scope: &chargepilot_domain::operations::DataScope,
    filters: &serde_json::Value,
    limit: i64,
) -> Result<Vec<serde_json::Value>, ExportError> {
    use crate::registry::scope_filter;
    use chargepilot_domain::operations;
    use sqlx::Row;

    let columns: Vec<&str> = spec.columns.iter().map(|col| col.column).collect();
    let column_list = columns
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut where_parts: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    // 账单是不可软删的流水表，其余资源统一排除已删除行。
    if spec.table != "charge_bill" {
        where_parts.push("`deleted_at` IS NULL".to_string());
    }
    if spec.table == "charge_bill" {
        let from = filters.get("from").and_then(|v| v.as_str()).unwrap_or("");
        let to = filters.get("to").and_then(|v| v.as_str()).unwrap_or("");
        where_parts.push("`issued_at` >= ? AND `issued_at` < ?".to_string());
        binds.push(from.to_string());
        binds.push(operations::next_export_date(to));
    }
    let (scope_clause, scope_binds) = scope_filter(spec, scope);
    if !scope_clause.is_empty() {
        where_parts.push(scope_clause);
    }
    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };
    let sql = format!(
        "SELECT {column_list} FROM `{}`{where_sql} LIMIT ?",
        spec.table
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for b in &scope_binds {
        q = q.bind(*b);
    }
    q = q.bind(limit);
    let rows = q.fetch_all(c).await?;
    // 按列名重组为 JSON 对象（json 键 = 列名，值为文本/数字/空）。
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::new();
        for (idx, column) in columns.iter().enumerate() {
            let value = row
                .try_get::<Option<serde_json::Value>, _>(idx)
                .unwrap_or(None)
                .unwrap_or(serde_json::Value::Null);
            obj.insert((*column).to_string(), value);
        }
        out.push(serde_json::Value::Object(obj));
    }
    Ok(out)
}
