//! 导出文件的生成、发布与校验（旧版 artifact.go + format.go 的单体对应物）。
//! 内容文件与私有校验文件先写 .partial 再改名发布；下载核对校验文件的
//! 任务、范围、脱敏与 SHA-256 摘要。文件名只由可信任务号推导。

use crate::registry::{ExportSpec, contains_scope, effective_scope};
use chargepilot_domain::operations::{self, DataScope, FieldMask};
use rust_xlsxwriter::Workbook;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 服务生成的任务号形态（EXP + 至多 32 位大写十六进制）。
fn valid_task_no(task_no: &str) -> bool {
    let Some(rest) = task_no.strip_prefix("EXP") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase())
}

/// 按格式和合法任务号生成内容文件与校验文件名（不含目录）。
/// 历史 PDF 仅允许清理；下载只支持 CSV/XLSX。
pub fn file_names(task_no: &str, format: &str, cleanup: bool) -> Result<(String, String), ()> {
    let format = if format.is_empty() {
        operations::ExportFileFormat::CSV
    } else {
        format
    };
    if !valid_task_no(task_no) {
        return Err(());
    }
    let known = operations::ExportFileFormat(format.to_string()).known();
    if !known && !(cleanup && format == operations::ExportFileFormat::LEGACY_PDF) {
        return Err(());
    }
    Ok((
        format!("{task_no}.{format}"),
        format!("{task_no}.scope.json"),
    ))
}

/// 私有校验文件（旧版 artifactProof）：只有 completed 且任务、摘要、范围、
/// 脱敏规则均匹配时才可下载。
#[derive(Debug, Serialize, Deserialize)]
pub struct ArtifactProof {
    pub version: u32,
    pub task_no: String,
    pub resource: String,
    pub format: String,
    pub requested_by: u64,
    pub row_count: u32,
    pub scope: DataScope,
    pub masked_columns: Vec<String>,
    pub sha256: String,
}

// serde derive 导入（Deserialize 用于校验文件读取）。
use serde::Deserialize;

/// 校验失败分类：Unavailable→HTTP 409、Forbidden→403（旧版 DownloadError /
/// ForbiddenError 的区分由调用方映射状态码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    /// 文件缺失、路径不符或校验不通过。
    Unavailable,
    /// 当前范围或脱敏规则无权访问该文件。
    Forbidden,
}

/// CSV 单元格转义（RFC4180：含逗号/引号/换行/CRLF 时加引号并双写引号）。
fn csv_cell(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// 编码 CSV（含 BOM 兼容 Excel 中文表头？——旧版不加 BOM，保持一致）。
fn encode_csv(headers: &[&str], lines: &[Vec<String>]) -> Vec<u8> {
    let mut out = String::new();
    let head: Vec<String> = headers.iter().map(|h| csv_cell(h)).collect();
    out.push_str(&head.join(","));
    out.push_str("\r\n");
    for line in lines {
        let cells: Vec<String> = line.iter().map(|c| csv_cell(c)).collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// 编码 XLSX（rust_xlsxwriter）：全单元格以文本写入（公式转义由
/// safe_spreadsheet_cell 保证），行列与 CSV 一致。
fn encode_xlsx(headers: &[&str], lines: &[Vec<String>]) -> anyhow::Result<Vec<u8>> {
    let mut book = Workbook::new();
    let sheet = book.add_worksheet();
    let mut all_rows: Vec<Vec<String>> = Vec::with_capacity(lines.len() + 1);
    all_rows.push(headers.iter().map(|h| h.to_string()).collect());
    all_rows.extend(lines.iter().cloned());
    for (row, line) in all_rows.iter().enumerate() {
        for (col, value) in line.iter().enumerate() {
            sheet.write_string(row as u32, col as u16, value)?;
        }
    }
    Ok(book.save_to_buffer()?)
}

/// 查询行按列取值转文本（旧版 stringifyCell）。
fn cell_text(record: &serde_json::Value, column: &str) -> String {
    let value = &record[column];
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(v) => v.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// 发布导出文件：生成内容与校验文件（先 .partial 再改名），返回内容文件
/// 相对路径（v1/<name>）。调用方须在任务锁事务内写完成状态。
pub fn publish(
    root: &Path,
    task_no: &str,
    resource: &str,
    format: &str,
    requested_by: u64,
    spec: &ExportSpec,
    records: &[serde_json::Value],
    scope: &DataScope,
    mask: &FieldMask,
) -> anyhow::Result<String> {
    let (name, proof_name) =
        file_names(task_no, format, false).map_err(|()| anyhow::anyhow!("导出文件名不可用"))?;
    std::fs::create_dir_all(root.join("v1"))?;
    let v1_name = PathBuf::from("v1").join(&name);
    let v1_proof = PathBuf::from("v1").join(&proof_name);
    let v1_name_str = v1_name.to_string_lossy().into_owned();
    let v1_proof_str = v1_proof.to_string_lossy().into_owned();

    // 组装行（脱敏替换在先，公式防护在后）。
    let headers: Vec<&str> = spec.columns.iter().map(|c| c.header).collect();
    let mut masked = Vec::new();
    for column in spec.columns {
        if mask.hide(spec.table, column.column) {
            masked.push(column.column.to_string());
        }
    }
    let mut lines = Vec::with_capacity(records.len());
    for record in records {
        let mut line = Vec::with_capacity(spec.columns.len());
        for column in spec.columns {
            let mut value = cell_text(record, column.column);
            if mask.hide(spec.table, column.column) {
                value = operations::MASK_PLACEHOLDER.to_string();
            }
            line.push(operations::safe_spreadsheet_cell(&value));
        }
        lines.push(line);
    }

    let content: Vec<u8> = match format {
        operations::ExportFileFormat::XLSX => encode_xlsx(&headers, &lines)?,
        _ => encode_csv(&headers, &lines),
    };
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let digest = format!("{:x}", hasher.finalize());

    let proof = ArtifactProof {
        version: 1,
        task_no: task_no.to_string(),
        resource: resource.to_string(),
        format: format.to_string(),
        requested_by,
        row_count: records.len() as u32,
        scope: effective_scope(resource, scope),
        masked_columns: masked,
        sha256: digest,
    };
    let proof_bytes = serde_json::to_vec(&proof)?;

    // 先写 .partial 再改名发布（同名残留被覆盖）。
    std::fs::write(root.join(format!("{v1_name_str}.partial")), &content)?;
    std::fs::write(root.join(format!("{v1_proof_str}.partial")), &proof_bytes)?;
    std::fs::rename(
        root.join(format!("{v1_name_str}.partial")),
        root.join(&v1_name),
    )?;
    std::fs::rename(
        root.join(format!("{v1_proof_str}.partial")),
        root.join(&v1_proof),
    )?;
    Ok(v1_name.to_string_lossy().into_owned())
}

/// 按合法任务号删除内容、校验与临时文件（根目录与 v1 两种布局）。
pub fn remove_artifacts(root: &Path, task_no: &str, format: &str) -> anyhow::Result<()> {
    let Ok((name, proof)) = file_names(task_no, format, true) else {
        anyhow::bail!("导出文件名不可用");
    };
    let mut combined = Ok(());
    for rel in [
        name.clone(),
        proof.clone(),
        format!("{name}.partial"),
        format!("{proof}.partial"),
        format!("v1/{name}"),
        format!("v1/{proof}"),
        format!("v1/{name}.partial"),
        format!("v1/{proof}.partial"),
    ] {
        let path = root.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => combined = Err(anyhow::anyhow!("删除 {path:?} 失败: {e}")),
        }
    }
    combined
}

/// 下载校验结果：内容字节与建议下载名。
pub struct VerifiedArtifact {
    pub content: Vec<u8>,
    pub name: String,
}

/// 校验已发布文件：路径与数据库一致、校验文件任务/范围/脱敏匹配、SHA-256 一致
/// （旧版 verifyArtifact + expectedPath；versioned 恒为 true——新仓库只发布 v1）。
pub fn verify_artifact(
    root: &Path,
    export_dir: &str,
    task_no: &str,
    resource: &str,
    format: &str,
    requested_by: u64,
    row_count: u32,
    db_path: &str,
    spec: &ExportSpec,
    scope: &DataScope,
    mask: &FieldMask,
) -> Result<VerifiedArtifact, ArtifactError> {
    let Ok((name, proof_name)) = file_names(task_no, format, false) else {
        return Err(ArtifactError::Unavailable);
    };
    let expected = format!("{export_dir}/v1/{name}");
    if db_path != expected {
        return Err(ArtifactError::Unavailable);
    }
    let v1_name = PathBuf::from("v1").join(&name);
    let v1_proof = PathBuf::from("v1").join(&proof_name);
    let content = match std::fs::read(root.join(&v1_name)) {
        Ok(c) => c,
        Err(_) => return Err(ArtifactError::Unavailable),
    };
    let proof_bytes = match std::fs::read(root.join(&v1_proof)) {
        Ok(p) => p,
        // 缺校验文件一律拒绝（新仓库只有 v1 布局，无旧根目录兼容规则）。
        Err(_) => return Err(ArtifactError::Unavailable),
    };
    if proof_bytes.len() > 64 * 1024 {
        return Err(ArtifactError::Unavailable);
    }
    let Ok(proof) = serde_json::from_slice::<ArtifactProof>(&proof_bytes) else {
        return Err(ArtifactError::Unavailable);
    };
    if proof.version != 1
        || proof.task_no != task_no
        || proof.resource != resource
        || proof.format != format
        || proof.requested_by != requested_by
        || proof.row_count != row_count
    {
        return Err(ArtifactError::Unavailable);
    }
    if !contains_scope(&effective_scope(resource, scope), &proof.scope) {
        return Err(ArtifactError::Forbidden);
    }
    for column in spec.columns {
        if mask.hide(spec.table, column.column)
            && !proof.masked_columns.iter().any(|c| c == column.column)
        {
            return Err(ArtifactError::Forbidden);
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(&content);
    if format!("{:x}", hasher.finalize()) != proof.sha256 {
        return Err(ArtifactError::Unavailable);
    }
    Ok(VerifiedArtifact { content, name })
}
