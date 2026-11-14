//! 导出任务服务（旧版 service.go + generation.go 的单体对应物）：
//! 受理（同事务登记+锁定权限核对+重放比对）、同步生成（取数→发布→完成）、
//! 列表/详情、下载（归属/范围/脱敏/有效期核对）。

use crate::artifact;
use crate::registry::{ExportSpec, export_resource, export_resource_names, scope_filter};
use chargepilot_domain::operations::{self, DataScope, ExportFileFormat, ExportStatus};
use chargepilot_mod_ops::{Profile, repo as ops_repo, scope as ops_scope};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Acquire, MySqlPool, Row};

/// 导出域错误（HTTP 层按变体映射旧版码值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// 请求参数不合法（400/1005，文案透出）。
    Input(String),
    /// 权限、归属或范围不足（403/1003，文案透出）。
    Forbidden(String),
    /// 登录凭据已变化（401）。
    Credentials,
    /// 当前状态不允许下载；过期返回 410，其余 409。
    Download { message: String, expired: bool },
    /// 记录不存在（404）。
    NotFound,
    /// 冲突（409/2009）。
    Conflict,
    /// 存储错误（503/5003）。
    Storage(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Input(m) => f.write_str(m),
            ExportError::Forbidden(m) => f.write_str(m),
            ExportError::Credentials => f.write_str("credentials invalid"),
            ExportError::Download { message, .. } => f.write_str(message),
            ExportError::NotFound => f.write_str("记录不存在或已删除"),
            ExportError::Conflict => f.write_str("导出请求冲突"),
            ExportError::Storage(m) => write!(f, "导出存储错误: {m}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<sqlx::Error> for ExportError {
    fn from(e: sqlx::Error) -> Self {
        ExportError::Storage(e.to_string())
    }
}

impl From<anyhow::Error> for ExportError {
    fn from(e: anyhow::Error) -> Self {
        ExportError::Storage(e.to_string())
    }
}

/// 导出申请（旧版 CreateInput）：空格式默认 CSV。
#[derive(Debug, Clone)]
pub struct CreateInput {
    /// 完整 UUID，任务号。
    pub request_id: String,
    /// 已登记只读资源。
    pub resource: String,
    /// csv/xlsx，空值默认 csv。
    pub format: String,
    /// 筛选（账单限定 from/to）。
    pub filter: serde_json::Value,
}

/// 受理或重放结果（旧版 CreateResult）。
#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub task_no: String,
    pub status: String,
    pub row_count: u32,
    pub expires_at: Option<String>,
    /// 非空表示已持久失败，HTTP 返回 failed 与公开提示。
    pub message: String,
}

/// 可导出资源清单（旧版 ResourcesResult）。
#[derive(Debug, Clone, Serialize)]
pub struct ResourcesResult {
    pub items: Vec<String>,
    pub formats: Vec<String>,
    pub max_rows: u32,
}

/// 导出任务行（旧版 ExportTaskRow；file_path 不出网）。
#[derive(Debug, Clone, Serialize)]
pub struct ExportTaskRow {
    pub id: u64,
    pub task_no: String,
    pub resource: String,
    pub file_format: String,
    pub status: String,
    pub requested_by: u64,
    pub filter_json: Option<String>,
    pub row_count: u32,
    pub error_msg: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: Option<String>,
    pub completed_at: Option<String>,
}

pub(crate) type TaskTup = (
    u64,
    String,
    String,
    String,
    String,
    u64,
    Option<String>,
    u32,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(crate) const TASK_COLUMNS: &str = "`id`, `task_no`, `resource`, `file_format`, `status`, `requested_by`, \
     CAST(`filter_json` AS CHAR) AS `filter_json`, `row_count`, `error_msg`, \
     CAST(`expires_at` AS CHAR) AS `expires_at`, CAST(`created_at` AS CHAR) AS `created_at`, \
     CAST(`completed_at` AS CHAR) AS `completed_at`";

pub(crate) fn task_from(t: TaskTup) -> ExportTaskRow {
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
    ExportTaskRow {
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

/// 导出服务配置（旧版 ExportService 的配置部分）。
#[derive(Debug, Clone, Default)]
pub struct ExportConfig {
    /// 导出根目录；空值使用系统临时目录下的 chargepilot-exports。
    pub export_dir: String,
    /// 单次上限，非正数默认 50000。
    pub max_rows: i64,
}

impl ExportConfig {
    /// 单次导出行数上限。
    #[must_use]
    pub fn max_rows(&self) -> i64 {
        if self.max_rows > 0 {
            self.max_rows
        } else {
            50_000
        }
    }

    /// 配置的可信根目录，不采用数据库路径或请求参数。
    #[must_use]
    pub fn export_dir(&self) -> String {
        if self.export_dir.is_empty() {
            std::env::temp_dir()
                .join("chargepilot-exports")
                .to_string_lossy()
                .into_owned()
        } else {
            self.export_dir.clone()
        }
    }
}

fn is_uuid(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, ch) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *ch != b'-' {
                    return false;
                }
            }
            _ => {
                if !ch.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// 锁定权限核对（旧版 lockedActor）：角色集合锁 → 账号锁 → 角色锁 → 权限码
/// FOR SHARE；auth_version 变化即凭据失效。
async fn locked_actor(
    c: &mut sqlx::MySqlConnection,
    original: &Profile,
    permission: &str,
) -> Result<Profile, ExportError> {
    ops_repo::lock_customer_admin_role_tx(&mut *c)
        .await
        .map_err(ExportError::from)?;
    let Some(account) = ops_repo::lock_active_admin_tx(&mut *c, original.id).await? else {
        return Err(ExportError::Credentials);
    };
    if account.auth_version != original.auth_version {
        return Err(ExportError::Credentials);
    }
    let Some(role_id) = account.role_id else {
        return Err(ExportError::Credentials);
    };
    let Some((_, role_code)) = ops_repo::lock_role_tx(&mut *c, role_id).await? else {
        return Err(ExportError::Credentials);
    };
    let mut codes = ops_repo::role_permission_codes_tx(&mut *c, role_id).await?;
    codes.sort();
    let mut p = Profile {
        auth_version: account.auth_version,
        id: account.id,
        username: account.username,
        display_name: account.display_name.unwrap_or_default(),
        role: role_code,
        role_name: String::new(),
        role_id,
        mfa_enabled: false,
        permissions: codes,
    };
    p.permissions.sort();
    if !p.permissions.iter().any(|x| x == permission) {
        return Err(ExportError::Forbidden("没有导出该资源的权限".into()));
    }
    Ok(p)
}

/// 可导出资源清单：核对 export.create 后按权限与范围过滤
/// （旧版 Resources；bills 仅全局范围可见）。
pub async fn list_resources(
    pool: &MySqlPool,
    original: &Profile,
    config: &ExportConfig,
) -> Result<ResourcesResult, ExportError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let p = locked_actor(&mut *c, original, "export.create").await?;
    let scope = ops_scope::load_data_scope_tx(&mut *c, &p, false).await?;
    tx.commit().await?;
    let mut items = Vec::new();
    for name in export_resource_names() {
        let Some(spec) = export_resource(name) else {
            continue;
        };
        let permitted = p.permissions.iter().any(|x| x == spec.permission);
        if permitted && (scope.unrestricted || *name != "bills") {
            items.push((*name).to_string());
        }
    }
    Ok(ResourcesResult {
        items,
        formats: vec![
            ExportFileFormat::CSV.to_string(),
            ExportFileFormat::XLSX.to_string(),
        ],
        max_rows: config.max_rows() as u32,
    })
}

/// 全局任务分页（旧版 List）：finance.read 权限。
pub async fn list_tasks(
    pool: &MySqlPool,
    original: &Profile,
    status: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<ExportTaskRow>, i64), ExportError> {
    if !(1..=1_000_000).contains(&page) || !(1..=100).contains(&page_size) {
        return Err(ExportError::Input("分页参数无效".into()));
    }
    if !status.is_empty() && !ExportStatus(status.to_string()).known() {
        return Err(ExportError::Input("关键词过长或状态无效".into()));
    }
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    locked_actor(&mut *c, original, "finance.read").await?;
    let total: i64 = {
        let sql = if status.is_empty() {
            "SELECT COUNT(*) FROM `export_task`".to_string()
        } else {
            "SELECT COUNT(*) FROM `export_task` WHERE `status` = ?".to_string()
        };
        let mut q = sqlx::query_scalar::<_, i64>(&sql);
        if !status.is_empty() {
            q = q.bind(status);
        }
        q.fetch_one(&mut *c).await?
    };
    let rows: Vec<TaskTup> = {
        let sql = if status.is_empty() {
            format!("SELECT {TASK_COLUMNS} FROM `export_task` ORDER BY `id` DESC LIMIT ? OFFSET ?")
        } else {
            format!(
                "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `status` = ? \
                 ORDER BY `id` DESC LIMIT ? OFFSET ?"
            )
        };
        let mut q = sqlx::query_as::<_, TaskTup>(&sql);
        if !status.is_empty() {
            q = q.bind(status);
        }
        q = q.bind(page_size).bind((page - 1) * page_size);
        q.fetch_all(&mut *c).await?
    };
    tx.commit().await?;
    Ok((rows.into_iter().map(task_from).collect(), total))
}

/// 任务详情（旧版 Detail）：finance.read 权限。
pub async fn detail(
    pool: &MySqlPool,
    original: &Profile,
    id: u64,
) -> Result<ExportTaskRow, ExportError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    locked_actor(&mut *c, original, "finance.read").await?;
    let row: Option<TaskTup> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `id` = ?"
    ))
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await?;
    tx.commit().await?;
    row.map(task_from).ok_or(ExportError::NotFound)
}

/// 校验申请（旧版 validate）：UUID、资源、格式、账单时间范围。
fn validate(input: &CreateInput) -> Result<(ExportSpec, String), ExportError> {
    if !is_uuid(&input.request_id) {
        return Err(ExportError::Input("请提供 UUID 格式的请求号".into()));
    }
    let Some(spec) = export_resource(&input.resource) else {
        return Err(ExportError::Input("不支持的导出资源".into()));
    };
    let format = if input.format.is_empty() {
        ExportFileFormat::CSV.to_string()
    } else {
        input.format.clone()
    };
    if !ExportFileFormat(format.clone()).known() {
        return Err(ExportError::Input("导出格式无效".into()));
    }
    if input.resource == "bills" && !operations::valid_export_period(&input.filter) {
        return Err(ExportError::Input(
            "账单导出需提供不超过 31 天的 from/to 日期".into(),
        ));
    }
    Ok((spec, format))
}

/// 受理并同步生成（旧版 Create）：登记、重放比对与首次审计同事务；
/// pending/running 继续生成；failed/completed 重放不换任务号。
pub async fn create_export(
    pool: &MySqlPool,
    original: &Profile,
    client_ip: &str,
    request_id: &str,
    input: &CreateInput,
    config: &ExportConfig,
) -> Result<CreateResult, ExportError> {
    let (spec, format) = validate(input)?;
    let filter_json = serde_json::to_string(&input.filter)
        .map_err(|_| ExportError::Input("导出筛选条件无效".into()))?;
    let task_no = format!("EXP{}", input.request_id.replace('-', "").to_uppercase());

    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let p = locked_actor(&mut *c, original, "export.create").await?;
    if !p.permissions.iter().any(|x| x == spec.permission) {
        return Err(ExportError::Forbidden("没有导出该资源的权限".into()));
    }
    let scope = ops_scope::load_data_scope_tx(&mut *c, &p, true).await?;
    if input.resource == "bills" && !scope.unrestricted {
        return Err(ExportError::Forbidden(
            "该财务资源只允许全局数据范围导出".into(),
        ));
    }
    let expires_at = Utc::now() + chrono::Duration::hours(24);
    // 唯一键兜底并发登记（旧版 ClaimExportTask 的并发语义）：预检不存在才
    // 插入，撞 uk_task_no（23000）视为他人已登记。不使用 ON DUPLICATE KEY：
    // sqlx 连接的 FOUND_ROWS 语义使无变化重放同样返回 1，无法区分首插。
    let first_seen: Option<u64> =
        sqlx::query_scalar("SELECT `id` FROM `export_task` WHERE `task_no` = ?")
            .bind(&task_no)
            .fetch_optional(&mut *c)
            .await?;
    let inserted = if first_seen.is_none() {
        match sqlx::query(
            "INSERT INTO `export_task` (`task_no`, `resource`, `file_format`, `status`, \
             `requested_by`, `filter_json`, `expires_at`) VALUES (?, ?, ?, 'pending', ?, ?, ?)",
        )
        .bind(&task_no)
        .bind(&input.resource)
        .bind(&format)
        .bind(p.id as i64)
        .bind(&filter_json)
        .bind(expires_at)
        .execute(&mut *c)
        .await
        {
            Ok(_) => true,
            Err(e) => {
                if let sqlx::Error::Database(db) = &e {
                    if db.code().as_deref() == Some("23000") {
                        false
                    } else {
                        return Err(e.into());
                    }
                } else {
                    return Err(e.into());
                }
            }
        }
    } else {
        false
    };
    let row_tup: TaskTup = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ? FOR UPDATE"
    ))
    .bind(&task_no)
    .fetch_one(&mut *c)
    .await?;
    let row = task_from(row_tup);
    // 重放比对：申请人、资源、格式与筛选须一致（json 语义比较）。
    let saved_filter: serde_json::Value = row
        .filter_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    if row.requested_by != p.id
        || row.resource != input.resource
        || row.file_format != format
        || saved_filter != input.filter
    {
        return Err(ExportError::Conflict);
    }
    if inserted {
        chargepilot_mod_ops::repo::append_audit_tx(
            &mut *c,
            &p,
            "export_task",
            "create",
            "export_task",
            "0",
            &serde_json::Value::Null,
            &serde_json::json!({"resource": input.resource, "task_no": task_no}),
            client_ip,
            request_id,
        )
        .await?;
    }
    let needs_generate = row.status == ExportStatus::PENDING || row.status == ExportStatus::RUNNING;
    tx.commit().await?;

    if needs_generate && let Err(e) = generate(pool, &task_no, &p, config).await {
        // 生成失败已在库中标记 failed：HTTP 仍返回 200 + failed 结果
        // （旧版 generationFailure 语义）。
        if matches!(e, ExportError::Storage(_)) {
            let row = find_by_no(pool, &task_no).await?;
            let failed = row.status == ExportStatus::FAILED;
            return Ok(CreateResult {
                task_no,
                status: row.status,
                row_count: row.row_count,
                expires_at: row.expires_at,
                message: if failed {
                    "导出失败，请稍后重试".into()
                } else {
                    String::new()
                },
            });
        }
        return Err(e);
    }
    let row = find_by_no(pool, &task_no).await?;
    Ok(CreateResult {
        task_no,
        status: row.status,
        row_count: row.row_count,
        expires_at: row.expires_at,
        message: String::new(),
    })
}

async fn find_by_no(pool: &MySqlPool, task_no: &str) -> Result<ExportTaskRow, ExportError> {
    let row: Option<TaskTup> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ?"
    ))
    .bind(task_no)
    .fetch_optional(pool)
    .await?;
    row.map(task_from).ok_or(ExportError::NotFound)
}

/// 同步生成（旧版 Generate 主干）：锁定权限与任务 → 范围/脱敏 FOR SHARE →
/// 取数（上限+1 判超限）→ 发布文件 → 重查有效期 → completed。
/// 文件与 SQL 不保证原子，下载以校验文件兜底；失败路径由 settle_failure
/// 在独立事务中清理文件并标记 failed。
async fn generate(
    pool: &MySqlPool,
    task_no: &str,
    original: &Profile,
    config: &ExportConfig,
) -> Result<(), ExportError> {
    let result = generate_inner(pool, task_no, original, config).await;
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let failed = settle_failure(pool, task_no, original.id, &e, config).await?;
            if failed {
                // 已持久失败：调用方返回 failed 结果（Create 捕获 Storage 分支）。
                Err(ExportError::Storage(e.to_string()))
            } else {
                // 未标记（他人已完成等）：按原错误返回。
                Err(e)
            }
        }
    }
}

async fn generate_inner(
    pool: &MySqlPool,
    task_no: &str,
    original: &Profile,
    config: &ExportConfig,
) -> Result<(), ExportError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let p = locked_actor(&mut *c, original, "export.create").await?;
    let row_tup: Option<TaskTup> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ? FOR UPDATE"
    ))
    .bind(task_no)
    .fetch_optional(&mut *c)
    .await?;
    let Some(row_tup) = row_tup else {
        return Err(ExportError::NotFound);
    };
    let row = task_from(row_tup);
    if row.requested_by != p.id {
        return Err(ExportError::Forbidden("只能生成自己创建的导出文件".into()));
    }
    if row.status == ExportStatus::COMPLETED
        || row.status == ExportStatus::FAILED
        || row.status == ExportStatus::EXPIRED
    {
        return Ok(());
    }
    if row.status != ExportStatus::PENDING && row.status != ExportStatus::RUNNING {
        return Err(ExportError::Conflict);
    }
    let Some(spec) = export_resource(&row.resource) else {
        return Err(ExportError::Storage("未登记的导出资源".into()));
    };
    if !p.permissions.iter().any(|x| x == spec.permission) {
        return Err(ExportError::Forbidden("没有导出该资源的权限".into()));
    }
    let expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT `expires_at` FROM `export_task` WHERE `id` = ? FOR SHARE")
            .bind(row.id as i64)
            .fetch_one(&mut *c)
            .await?;
    if expiry.is_some_and(|e| Utc::now() >= e) {
        return Err(ExportError::Download {
            message: "导出文件已过期，请重新导出".into(),
            expired: true,
        });
    }
    let scope = ops_scope::load_data_scope_tx(&mut *c, &p, true).await?;
    if row.resource == "bills" && !scope.unrestricted {
        return Err(ExportError::Forbidden(
            "该财务资源只允许全局数据范围导出".into(),
        ));
    }
    let mask = ops_scope::load_field_mask_tx(&mut *c, p.role_id, true).await?;
    let filters: serde_json::Value = row
        .filter_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let records =
        query_export_source(&mut *c, &spec, &scope, &filters, config.max_rows() + 1).await?;
    if records.len() as i64 > config.max_rows() {
        return Err(ExportError::Input(format!(
            "导出超过 {} 行，请缩小范围",
            config.max_rows()
        )));
    }
    let root = config.export_dir();
    std::fs::create_dir_all(&root).map_err(anyhow::Error::from)?;
    let rel_path = artifact::publish(
        Path::new(&root),
        task_no,
        &row.resource,
        &row.file_format,
        row.requested_by,
        &spec,
        &records,
        &scope,
        &mask,
    )?;
    // 取数和编码后重查有效期（已发布字节不能作为过期任务完成的依据）。
    let now = Utc::now();
    if expiry.is_some_and(|e| now >= e) {
        return Err(ExportError::Download {
            message: "导出文件已过期，请重新导出".into(),
            expired: true,
        });
    }
    let updated = sqlx::query(
        "UPDATE `export_task` SET `status` = ?, `row_count` = ?, `file_path` = ?, \
         `completed_at` = ?, `error_msg` = NULL \
         WHERE `task_no` = ? AND `status` IN ('pending','running')",
    )
    .bind(ExportStatus::COMPLETED)
    .bind(records.len() as u32)
    .bind(format!("{root}/{rel_path}"))
    .bind(now)
    .bind(task_no)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(ExportError::Conflict);
    }
    tx.commit().await?;
    Ok(())
}

use std::path::Path;

/// 失败收尾（旧版 settleFailure）：锁读任务并核对申请人，清理自己的
/// 未完成任务文件并标记 failed；数据库不可用时保留 pending 供恢复。
async fn settle_failure(
    pool: &MySqlPool,
    task_no: &str,
    actor_id: u64,
    cause: &ExportError,
    config: &ExportConfig,
) -> Result<bool, ExportError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let row_tup: Option<TaskTup> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `task_no` = ? FOR UPDATE"
    ))
    .bind(task_no)
    .fetch_optional(&mut *c)
    .await?;
    let Some(row_tup) = row_tup else {
        return Ok(false);
    };
    let row = task_from(row_tup);
    if row.requested_by != actor_id {
        return Ok(false);
    }
    if row.status == ExportStatus::COMPLETED
        || row.status == ExportStatus::EXPIRED
        || row.status == ExportStatus::FAILED
    {
        return Ok(false);
    }
    let root = config.export_dir();
    artifact::remove_artifacts(Path::new(&root), &row.task_no, &row.file_format)?;
    let reason = truncate_reason(&cause.to_string(), 255);
    sqlx::query(
        "UPDATE `export_task` SET `status` = ?, `error_msg` = ? \
         WHERE `task_no` = ? AND `status` IN ('pending','running')",
    )
    .bind(ExportStatus::FAILED)
    .bind(reason)
    .bind(task_no)
    .execute(&mut *c)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(true)
}

/// 按字节上限截断错误文本，不截断 UTF-8 字符。
fn truncate_reason(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// 按登记资源读取业务数据（旧版 QueryExportSource）：依次过滤软删除、
/// 账单日期与数据范围；limit 为上限加一，多出的一行用于判断超限。
async fn query_export_source(
    c: &mut sqlx::MySqlConnection,
    spec: &ExportSpec,
    scope: &DataScope,
    filters: &serde_json::Value,
    limit: i64,
) -> Result<Vec<serde_json::Value>, ExportError> {
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
    let rows = q.fetch_all(&mut *c).await?;
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

/// 下载结果（旧版 Download）：内容字节与建议文件名。
pub struct DownloadFile {
    pub content: Vec<u8>,
    pub name: String,
    pub mime: &'static str,
}

/// 下载（旧版 GetDownload）：申请人或 admin_user.read 权限可下载；
/// 资源权限、范围、脱敏、有效期与校验文件全部核对。
pub async fn download(
    pool: &MySqlPool,
    original: &Profile,
    id: u64,
    config: &ExportConfig,
) -> Result<DownloadFile, ExportError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let p = locked_actor(&mut *c, original, "export.create").await?;
    let row_tup: Option<TaskTup> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM `export_task` WHERE `id` = ? FOR UPDATE"
    ))
    .bind(id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some(row_tup) = row_tup else {
        return Err(ExportError::NotFound);
    };
    // 服务器路径只内部比对（json:"-"，旧版语义）；同一行锁内读取。
    let file_path: Option<String> =
        sqlx::query_scalar("SELECT `file_path` FROM `export_task` WHERE `id` = ? FOR UPDATE")
            .bind(id as i64)
            .fetch_one(&mut *c)
            .await?;
    let row = task_from(row_tup);
    if row.requested_by != p.id && !p.permissions.iter().any(|x| x == "admin_user.read") {
        return Err(ExportError::Forbidden("只能下载自己创建的导出文件".into()));
    }
    let Some(spec) = export_resource(&row.resource) else {
        return Err(ExportError::Forbidden("没有下载该导出的权限".into()));
    };
    if !p.permissions.iter().any(|x| x == spec.permission) {
        return Err(ExportError::Forbidden("没有下载该导出的权限".into()));
    }
    let scope = ops_scope::load_data_scope_tx(&mut *c, &p, true).await?;
    if row.resource == "bills" && !scope.unrestricted {
        return Err(ExportError::Forbidden(
            "当前数据范围无权下载该财务导出".into(),
        ));
    }
    if row.status == ExportStatus::EXPIRED {
        return Err(ExportError::Download {
            message: "导出文件已过期，请重新导出".into(),
            expired: true,
        });
    }
    if row.status != ExportStatus::COMPLETED {
        return Err(ExportError::Download {
            message: "导出尚未完成".into(),
            expired: false,
        });
    }
    let Some(db_path) = file_path else {
        return Err(ExportError::Download {
            message: "导出尚未完成".into(),
            expired: false,
        });
    };
    let expiry: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT `expires_at` FROM `export_task` WHERE `id` = ? FOR SHARE")
            .bind(row.id as i64)
            .fetch_one(&mut *c)
            .await?;
    if expiry.is_some_and(|e| Utc::now() >= e) {
        crate::cleanup::expire_task(
            &mut *c,
            &config.export_dir(),
            &row.task_no,
            &row.file_format,
        )
        .await?;
        tx.commit().await?;
        return Err(ExportError::Download {
            message: "导出文件已过期，请重新导出".into(),
            expired: true,
        });
    }
    let mask = ops_scope::load_field_mask_tx(&mut *c, p.role_id, true).await?;
    tx.commit().await?;
    let root = config.export_dir();
    let verified = artifact::verify_artifact(
        Path::new(&root),
        &root,
        &row.task_no,
        &row.resource,
        &row.file_format,
        row.requested_by,
        row.row_count,
        &db_path,
        &spec,
        &scope,
        &mask,
    )
    .map_err(|kind| match kind {
        artifact::ArtifactError::Forbidden => {
            ExportError::Forbidden("当前数据范围无权下载该导出".into())
        }
        artifact::ArtifactError::Unavailable => ExportError::Download {
            message: "导出文件不可用，请重新导出".into(),
            expired: false,
        },
    })?;
    let mime = if row.file_format == ExportFileFormat::XLSX {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    } else {
        "text/csv; charset=utf-8"
    };
    Ok(DownloadFile {
        content: verified.content,
        name: verified.name,
        mime,
    })
}

/// 任务号内容摘要（供事件/日志使用；未参与旧版契约，保留内部用途）。
#[allow(dead_code)]
pub(crate) fn digest_of(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}
