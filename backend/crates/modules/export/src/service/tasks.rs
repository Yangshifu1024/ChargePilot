//! 导出任务用例：受理（同事务登记+锁定权限核对+重放比对）、同步生成、
//! 列表/详情、下载核对。SQL 原语在 repo/tasks.rs。

use crate::artifact;
use crate::error::{ArtifactError, ExportError};
use crate::registry::{ExportSpec, export_resource, export_resource_names};
use crate::repo::tasks;
use chargepilot_domain::export::ExportFileFormat;
use chargepilot_domain::operations;
use chargepilot_mod_ops::{Profile, repo as ops_repo, scope as ops_scope};
use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Acquire, MySqlPool};
use std::path::Path;

/// 导出申请：空格式默认 CSV。
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

/// 受理或重放结果。
#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub task_no: String,
    pub status: chargepilot_domain::export::ExportStatus,
    pub row_count: u32,
    pub expires_at: Option<String>,
    /// 非空表示已持久失败，HTTP 返回 failed 与公开提示。
    pub message: String,
}

/// 可导出资源清单。
#[derive(Debug, Clone, Serialize)]
pub struct ResourcesResult {
    pub items: Vec<String>,
    pub formats: Vec<String>,
    pub max_rows: u32,
}

/// 导出任务行（file_path 不出网）。
#[derive(Debug, Clone, Serialize)]
pub struct ExportTaskRow {
    pub id: u64,
    pub task_no: String,
    pub resource: String,
    pub file_format: String,
    pub status: chargepilot_domain::export::ExportStatus,
    pub requested_by: u64,
    pub filter_json: Option<String>,
    pub row_count: u32,
    pub error_msg: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: Option<String>,
    pub completed_at: Option<String>,
}

/// 导出服务配置。
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

/// 锁定权限核对：角色集合锁 → 账号锁 → 角色锁 → 权限码
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
/// （bills 仅全局范围可见）。
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
            ExportFileFormat::Csv.as_str().to_owned(),
            ExportFileFormat::Xlsx.as_str().to_owned(),
        ],
        max_rows: config.max_rows() as u32,
    })
}

/// 全局任务分页：finance.read 权限。
pub async fn list_tasks(
    pool: &MySqlPool,
    original: &Profile,
    status: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<ExportTaskRow>, i64), ExportError> {
    if chargepilot_domain::page::PageRequest::admin(page, page_size).is_err() {
        return Err(ExportError::Input("分页参数无效".into()));
    }
    use chargepilot_domain::export::ExportStatus;
    if !status.is_empty() && ExportStatus::parse(status).is_err() {
        return Err(ExportError::Input("关键词过长或状态无效".into()));
    }
    let status_filter = if status.is_empty() {
        None
    } else {
        Some(status)
    };
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    locked_actor(&mut *c, original, "finance.read").await?;
    let total = tasks::task_count_tx(&mut *c, status_filter).await?;
    let rows = tasks::list_task_tups_tx(&mut *c, status_filter, page, page_size).await?;
    tx.commit().await?;
    Ok((rows.into_iter().map(tasks::task_from).collect(), total))
}

/// 任务详情：finance.read 权限。
pub async fn detail(
    pool: &MySqlPool,
    original: &Profile,
    id: u64,
) -> Result<ExportTaskRow, ExportError> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    locked_actor(&mut *c, original, "finance.read").await?;
    let row = tasks::task_by_id_tx(&mut *c, id).await?;
    tx.commit().await?;
    row.map(tasks::task_from).ok_or(ExportError::NotFound)
}

/// 校验申请：UUID、资源、格式、账单时间范围。
fn validate(input: &CreateInput) -> Result<(ExportSpec, String), ExportError> {
    if !is_uuid(&input.request_id) {
        return Err(ExportError::Input("请提供 UUID 格式的请求号".into()));
    }
    let Some(spec) = export_resource(&input.resource) else {
        return Err(ExportError::Input("不支持的导出资源".into()));
    };
    let format = if input.format.is_empty() {
        ExportFileFormat::Csv.as_str().to_owned()
    } else {
        input.format.clone()
    };
    if ExportFileFormat::parse(&format).is_err() {
        return Err(ExportError::Input("导出格式无效".into()));
    }
    if input.resource == "bills" && !operations::valid_export_period(&input.filter) {
        return Err(ExportError::Input(
            "账单导出需提供不超过 31 天的 from/to 日期".into(),
        ));
    }
    Ok((spec, format))
}

/// 受理并同步生成：登记、重放比对与首次审计同事务；
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
    tasks::set_read_committed(&mut conn).await?;
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
    // 唯一键兜底并发登记：预检不存在才插入，撞 uk_task_no（23000）视为
    // 他人已登记。不使用 ON DUPLICATE KEY：sqlx 连接的 FOUND_ROWS 语义使
    // 无变化重放同样返回 1，无法区分首插。
    let first_seen = tasks::first_seen_task_id_tx(&mut *c, &task_no).await?;
    let inserted = if first_seen.is_none() {
        match tasks::insert_task_tx(
            &mut *c,
            &task_no,
            &input.resource,
            &format,
            p.id,
            &filter_json,
            expires_at,
        )
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
    let row_tup = tasks::lock_task_by_no_tx(&mut *c, &task_no).await?;
    let row = tasks::task_from(row_tup.ok_or(ExportError::NotFound)?);
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
        ops_repo::append_audit_tx(
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
    let needs_generate = row.status == chargepilot_domain::export::ExportStatus::Pending
        || row.status == chargepilot_domain::export::ExportStatus::Running;
    tx.commit().await?;

    if needs_generate && let Err(e) = generate(pool, &task_no, &p, config).await {
        // 生成失败已在库中标记 failed：HTTP 仍返回 200 + failed 结果。
        if matches!(e, ExportError::Storage(_)) {
            let row = find_by_no(pool, &task_no).await?;
            let failed = row.status == chargepilot_domain::export::ExportStatus::Failed;
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
    tasks::task_by_no(pool, task_no)
        .await?
        .map(tasks::task_from)
        .ok_or(ExportError::NotFound)
}

/// 同步生成主干：锁定权限与任务 → 范围/脱敏 FOR SHARE →
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
    tasks::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let p = locked_actor(&mut *c, original, "export.create").await?;
    let row_tup = tasks::lock_task_by_no_tx(&mut *c, task_no).await?;
    let row = tasks::task_from(row_tup.ok_or(ExportError::NotFound)?);
    if row.requested_by != p.id {
        return Err(ExportError::Forbidden("只能生成自己创建的导出文件".into()));
    }
    use chargepilot_domain::export::ExportStatus;
    if row.status == ExportStatus::Completed
        || row.status == ExportStatus::Failed
        || row.status == ExportStatus::Expired
    {
        return Ok(());
    }
    if row.status != ExportStatus::Pending && row.status != ExportStatus::Running {
        return Err(ExportError::Conflict);
    }
    let Some(spec) = export_resource(&row.resource) else {
        return Err(ExportError::Storage("未登记的导出资源".into()));
    };
    if !p.permissions.iter().any(|x| x == spec.permission) {
        return Err(ExportError::Forbidden("没有导出该资源的权限".into()));
    }
    let expiry = tasks::expiry_for_share_tx(&mut *c, row.id).await?;
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
        tasks::query_export_source(&mut *c, &spec, &scope, &filters, config.max_rows() + 1).await?;
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
    let updated = tasks::complete_task_tx(
        &mut *c,
        task_no,
        records.len() as u32,
        &format!("{root}/{rel_path}"),
        now,
    )
    .await?;
    if updated != 1 {
        return Err(ExportError::Conflict);
    }
    tx.commit().await?;
    Ok(())
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

/// 失败收尾：锁读任务并核对申请人，清理自己的
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
    let row_tup = tasks::lock_task_by_no_tx(&mut *c, task_no).await?;
    let Some(row_tup) = row_tup else {
        return Ok(false);
    };
    let row = tasks::task_from(row_tup);
    if row.requested_by != actor_id {
        return Ok(false);
    }
    use chargepilot_domain::export::ExportStatus;
    if row.status == ExportStatus::Completed
        || row.status == ExportStatus::Expired
        || row.status == ExportStatus::Failed
    {
        return Ok(false);
    }
    let root = config.export_dir();
    artifact::remove_artifacts(Path::new(&root), &row.task_no, &row.file_format)?;
    let reason = truncate_reason(&cause.to_string(), 255);
    tasks::fail_task_tx(&mut *c, task_no, &reason).await?;
    tx.commit().await?;
    Ok(true)
}

/// 下载结果：内容字节与建议文件名。
pub struct DownloadFile {
    pub content: Vec<u8>,
    pub name: String,
    pub mime: &'static str,
}

/// 下载：申请人或 admin_user.read 权限可下载；
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
    let row_tup = tasks::lock_task_by_id_tx(&mut *c, id).await?;
    let Some(row_tup) = row_tup else {
        return Err(ExportError::NotFound);
    };
    // 服务器路径只内部比对（json:"-"）；同一行锁内读取。
    let file_path = tasks::file_path_for_update_tx(&mut *c, id).await?;
    let row = tasks::task_from(row_tup);
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
    use chargepilot_domain::export::ExportStatus;
    if row.status == ExportStatus::Expired {
        return Err(ExportError::Download {
            message: "导出文件已过期，请重新导出".into(),
            expired: true,
        });
    }
    if row.status != ExportStatus::Completed {
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
    let expiry = tasks::expiry_for_share_tx(&mut *c, row.id).await?;
    if expiry.is_some_and(|e| Utc::now() >= e) {
        crate::service::cleanup::expire_task(
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
        ArtifactError::Forbidden => ExportError::Forbidden("当前数据范围无权下载该导出".into()),
        ArtifactError::Unavailable => ExportError::Download {
            message: "导出文件不可用，请重新导出".into(),
            expired: false,
        },
    })?;
    let mime = if row.file_format == ExportFileFormat::Xlsx.as_str() {
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

/// 任务号内容摘要（供事件/日志使用；保留内部用途）。
#[allow(dead_code)]
pub(crate) fn digest_of(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}
