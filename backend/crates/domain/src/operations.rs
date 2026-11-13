//! 后台运营纯规则与词表（旧版 domain/operations 的 scope/export/account 纯部分）。
//! 零框架依赖：只承载归并、校验与单元格防护，不触碰数据库与 HTTP。

/// admin_data_scope.scope_type 列的数据范围类型（station/vendor）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeType(pub String);

impl ScopeType {
    pub const STATION: &'static str = "station";
    pub const VENDOR: &'static str = "vendor";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::STATION | Self::VENDOR)
    }
}

/// 单个账号可配置的数据范围条数上限。
pub const MAX_SCOPE_ITEMS: usize = 500;

/// 账号的归并数据范围（旧版 DataScope）：无任何范围记录即不受限；
/// 角色权限仍需单独校验，不受限只表示不附加数据范围条件。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DataScope {
    /// 归并后的站点 ID 列表。
    pub station_ids: Vec<u64>,
    /// 归并后的厂商 ID 列表。
    pub vendor_ids: Vec<u64>,
    /// 是否不受限（无任何范围记录）。
    pub unrestricted: bool,
    /// 范围类型（导出凭据记录用；内存归并路径主动写空串）。
    #[serde(default)]
    pub scope_type: String,
    /// 范围对象 ID（导出凭据记录用）。
    #[serde(default)]
    pub scope_id: u64,
}

/// 由一组原始范围行归并出内存范围集合（旧版 NewDataScope）。空输入即不受限。
#[must_use]
pub fn new_data_scope(rows: &[(String, u64)]) -> DataScope {
    let mut scope = DataScope::default();
    if rows.is_empty() {
        scope.unrestricted = true;
        return scope;
    }
    for (scope_type, scope_id) in rows {
        match scope_type.as_str() {
            ScopeType::STATION => scope.station_ids.push(*scope_id),
            ScopeType::VENDOR => scope.vendor_ids.push(*scope_id),
            // 未知类型按旧版行为静默忽略，不报错。
            _ => {}
        }
    }
    scope
}

impl DataScope {
    /// 检查站点 ID；未限制范围或站点列表为空时放行。
    #[must_use]
    pub fn allows_station(&self, id: u64) -> bool {
        if self.unrestricted || self.station_ids.is_empty() {
            return true;
        }
        self.station_ids.contains(&id)
    }

    /// 检查厂商 ID；未限制范围或厂商列表为空时放行。
    #[must_use]
    pub fn allows_vendor(&self, id: u64) -> bool {
        if self.unrestricted || self.vendor_ids.is_empty() {
            return true;
        }
        self.vendor_ids.contains(&id)
    }
}

/// 角色字段脱敏规则（旧版 FieldMask）：命中即以占位符替换输出。
#[derive(Debug, Clone, Default)]
pub struct FieldMask {
    pub role_id: u64,
    /// (资源, 字段) 对。
    pub fields: Vec<(String, String)>,
}

/// 单元格脱敏占位符（旧版 MaskPlaceholder）。
pub const MASK_PLACEHOLDER: &str = "******";

impl FieldMask {
    /// 判断某资源的某列是否需要脱敏；规则缺失即不脱敏。
    #[must_use]
    pub fn hide(&self, resource: &str, field: &str) -> bool {
        self.fields.iter().any(|(r, f)| r == resource && f == field)
    }
}

/// export_task.status 列的导出任务状态词表（五值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportStatus(pub String);

impl ExportStatus {
    pub const PENDING: &'static str = "pending";
    pub const RUNNING: &'static str = "running";
    pub const COMPLETED: &'static str = "completed";
    pub const FAILED: &'static str = "failed";
    pub const EXPIRED: &'static str = "expired";

    pub fn known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::PENDING | Self::RUNNING | Self::COMPLETED | Self::FAILED | Self::EXPIRED
        )
    }
}

/// export_task.file_format 列的导出文件格式词表（csv/xlsx 两值；
/// 历史 pdf 只为清理识别保留，不计入合法集合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFileFormat(pub String);

impl ExportFileFormat {
    pub const CSV: &'static str = "csv";
    pub const XLSX: &'static str = "xlsx";
    /// 历史遗留 PDF：仅清理路径识别，受理与下载一律拒绝。
    pub const LEGACY_PDF: &'static str = "pdf";

    pub fn known(&self) -> bool {
        matches!(self.0.as_str(), Self::CSV | Self::XLSX)
    }
}

/// 导出允许的最大时间跨度（30 天）。
pub const MAX_EXPORT_PERIOD: chrono::Duration = chrono::Duration::hours(30 * 24);

/// 导出筛选项使用的日期格式。
pub const EXPORT_DATE_LAYOUT: &str = "%Y-%m-%d";

/// 校验导出日期区间：只接受 from、to 两个键，跨度不超过上限（旧版 ValidExportPeriod）。
#[must_use]
pub fn valid_export_period(filter: &serde_json::Value) -> bool {
    let Some(obj) = filter.as_object() else {
        return false;
    };
    if obj.len() != 2 {
        return false;
    }
    let (Some(from_raw), Some(to_raw)) = (
        obj.get("from").and_then(|v| v.as_str()),
        obj.get("to").and_then(|v| v.as_str()),
    ) else {
        return false;
    };
    let (Ok(from), Ok(to)) = (
        chrono::NaiveDate::parse_from_str(from_raw, EXPORT_DATE_LAYOUT),
        chrono::NaiveDate::parse_from_str(to_raw, EXPORT_DATE_LAYOUT),
    ) else {
        return false;
    };
    let span = to.signed_duration_since(from);
    span >= chrono::Duration::zero() && span <= MAX_EXPORT_PERIOD
}

/// 返回结束日的下一天文本，供 issued_at < 上界 的查询包含整个结束日
/// （旧版 NextExportDate）；无法解析时原样返回，由调用方拒绝。
#[must_use]
pub fn next_export_date(raw: &str) -> String {
    match chrono::NaiveDate::parse_from_str(raw, EXPORT_DATE_LAYOUT) {
        Ok(d) => (d + chrono::Duration::days(1))
            .format(EXPORT_DATE_LAYOUT)
            .to_string(),
        Err(_) => raw.to_string(),
    }
}

/// 防止导出文本被表格软件当成公式（旧版 SafeSpreadsheetCell）：
/// 以 = + - @ 开头且不能解析为数字的内容加单引号，合法数字保持原样。
#[must_use]
pub fn safe_spreadsheet_cell(value: &str) -> String {
    if value.is_empty() {
        return value.to_string();
    }
    let first = value.as_bytes()[0];
    if matches!(first, b'=' | b'+' | b'-' | b'@') && value.parse::<f64>().is_err() {
        return format!("'{value}");
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_merge_and_checks() {
        // 空范围即不受限。
        let s = new_data_scope(&[]);
        assert!(s.unrestricted);
        assert!(s.allows_station(1));
        // 站点与厂商归并。
        let s = new_data_scope(&[
            ("station".into(), 1),
            ("station".into(), 3),
            ("vendor".into(), 9),
        ]);
        assert!(!s.unrestricted);
        assert!(s.allows_station(1));
        assert!(!s.allows_station(2));
        assert!(s.allows_vendor(9));
        assert!(!s.allows_vendor(8));
        // 未知类型静默忽略。
        let s = new_data_scope(&[("region".into(), 5)]);
        assert!(!s.unrestricted);
        assert!(s.allows_station(1), "无站点限制即放行站点");
    }

    #[test]
    fn field_mask_hide() {
        let mut mask = FieldMask {
            role_id: 7,
            fields: vec![],
        };
        assert!(!mask.hide("charge_bill", "user_id"));
        mask.fields.push(("charge_bill".into(), "user_id".into()));
        assert!(mask.hide("charge_bill", "user_id"));
        assert!(!mask.hide("charge_order", "user_id"));
    }

    #[test]
    fn export_period_and_cell_safety() {
        let ok = serde_json::json!({"from": "2026-01-01", "to": "2026-01-31"});
        assert!(valid_export_period(&ok));
        // 32 天超限。
        let long = serde_json::json!({"from": "2026-01-01", "to": "2026-02-02"});
        assert!(!valid_export_period(&long));
        // 多余键、缺键、倒序。
        assert!(!valid_export_period(
            &serde_json::json!({"from": "2026-01-01"})
        ));
        assert!(!valid_export_period(
            &serde_json::json!({"from": "2026-01-02", "to": "2026-01-01"})
        ));
        // 次日上界。
        assert_eq!(next_export_date("2026-01-31"), "2026-02-01");
        assert_eq!(next_export_date("bad"), "bad");
        // 公式防护：数字保持，文本加引号。
        assert_eq!(safe_spreadsheet_cell("=cmd()"), "'=cmd()");
        assert_eq!(safe_spreadsheet_cell("-123"), "-123");
        assert_eq!(safe_spreadsheet_cell("@x"), "'@x");
        assert_eq!(safe_spreadsheet_cell("普通"), "普通");
        assert_eq!(safe_spreadsheet_cell(""), "");
    }

    #[test]
    fn vocabularies() {
        assert!(ExportStatus("completed".into()).known());
        assert!(!ExportStatus("done".into()).known());
        assert!(ExportFileFormat("xlsx".into()).known());
        assert!(
            !ExportFileFormat("pdf".into()).known(),
            "历史 PDF 不在合法集合"
        );
        assert!(ScopeType("station".into()).known());
        assert!(!ScopeType("region".into()).known());
    }
}
