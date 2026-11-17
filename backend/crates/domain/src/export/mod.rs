//! 导出域共享词表。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// export_task.status 列的导出任务状态（五值）。
    ExportStatus: {
        Pending = "pending",
        Running = "running",
        Completed = "completed",
        Failed = "failed",
        Expired = "expired",
    }
);

wordlist_enum!(
    /// export_task.file_format 列的导出文件格式（csv/xlsx 两值）。
    /// 历史 pdf 遗留行不在词表：读取路径保持 String 行模型兼容（见
    /// modules/export 的行模型注释），受理与下载一律经本枚举 parse 拒绝。
    ExportFileFormat: {
        Csv = "csv",
        Xlsx = "xlsx",
    }
);

/// 历史遗留 PDF：仅清理路径识别，不属合法受理集合（保留为常量）。
pub const LEGACY_PDF: &str = "pdf";

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let status = [
            (ExportStatus::Pending, "pending"),
            (ExportStatus::Running, "running"),
            (ExportStatus::Completed, "completed"),
            (ExportStatus::Failed, "failed"),
            (ExportStatus::Expired, "expired"),
        ];
        for (v, s) in status {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let format = [
            (ExportFileFormat::Csv, "csv"),
            (ExportFileFormat::Xlsx, "xlsx"),
        ];
        for (v, s) in format {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
