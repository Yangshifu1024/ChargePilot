//! 设备域共享词表（票 10）：故障报告、告警、导入、设备与端口、站点、
//! 会话与厂商协议。协议故障码到故障类型的映射属协议层封闭语义（不在本域）。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// device_fault_report.report_source 列的故障上报来源（三值）。
    FaultReportSource: {
        User = "user",
        Inspect = "inspect",
        Monitor = "monitor",
    }
);

wordlist_enum!(
    /// device_fault_report.fault_type 列的故障类型（五值）。
    FaultType: {
        Mechanical = "mechanical",
        Electrical = "electrical",
        Communication = "communication",
        Display = "display",
        Other = "other",
    }
);

wordlist_enum!(
    /// device_fault_report.status 列的故障工单状态（四值）。
    FaultReportStatus: {
        Open = "open",
        Dispatched = "dispatched",
        Fixed = "fixed",
        Closed = "closed",
    }
);

wordlist_enum!(
    /// device_fault_report_event.event_type 列的工单事件类型（六值；
    /// migration_baseline 仅由迁移种子写入）。
    FaultReportEventType: {
        Reported = "reported",
        Dispatched = "dispatched",
        Reassigned = "reassigned",
        Fixed = "fixed",
        Closed = "closed",
        MigrationBaseline = "migration_baseline",
    }
);

wordlist_enum!(
    /// alert_event.severity 列的告警级别（三值）。
    AlertSeverity: {
        Warning = "warning",
        Critical = "critical",
        Fatal = "fatal",
    }
);

wordlist_enum!(
    /// alert_event.status 列的告警状态（四值；auto_resolved 为设备恢复自动闭环）。
    AlertStatus: {
        Active = "active",
        Acknowledged = "acknowledged",
        Resolved = "resolved",
        AutoResolved = "auto_resolved",
    }
);

wordlist_enum!(
    /// device_import.status 列的导入任务状态（三值）。
    DeviceImportStatus: {
        Pending = "pending",
        Completed = "completed",
        Failed = "failed",
    }
);

wordlist_enum!(
    /// device_meta.status 列的设备资料状态（四值；fault 由告警闭环联动）。
    DeviceMetaStatus: {
        Enabled = "enabled",
        Disabled = "disabled",
        Retired = "retired",
        Fault = "fault",
    }
);

wordlist_enum!(
    /// device.status 列的运行设备状态（三值；无 fault——故障仅资料态承载）。
    DeviceStatus: {
        Enabled = "enabled",
        Disabled = "disabled",
        Retired = "retired",
    }
);

wordlist_enum!(
    /// device_port.status 列的端口状态（五值；full 仅 SQL 端内部态）。
    DevicePortStatus: {
        Idle = "idle",
        Charging = "charging",
        Full = "full",
        Fault = "fault",
        Disabled = "disabled",
    }
);

wordlist_enum!(
    /// station.status 列的站点状态（三值）。
    StationStatus: {
        Active = "active",
        Disabled = "disabled",
        Construction = "construction",
    }
);

wordlist_enum!(
    /// device_session.protocol 列的接入协议（两值）。
    SessionProtocol: {
        Tcp = "tcp",
        Mqtt = "mqtt",
    }
);

wordlist_enum!(
    /// vendor.protocol 列的厂商接入协议（三值；hybrid 表示双通道）。
    VendorProtocol: {
        Tcp = "tcp",
        Mqtt = "mqtt",
        Hybrid = "hybrid",
    }
);

wordlist_enum!(
    /// vendor.status 列的厂商状态（两值）。
    VendorStatus: {
        Enabled = "enabled",
        Disabled = "disabled",
    }
);

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照（票 10 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let source = [
            (FaultReportSource::User, "user"),
            (FaultReportSource::Inspect, "inspect"),
            (FaultReportSource::Monitor, "monitor"),
        ];
        for (v, s) in source {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let ftype = [
            (FaultType::Mechanical, "mechanical"),
            (FaultType::Electrical, "electrical"),
            (FaultType::Communication, "communication"),
            (FaultType::Display, "display"),
            (FaultType::Other, "other"),
        ];
        for (v, s) in ftype {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let fstatus = [
            (FaultReportStatus::Open, "open"),
            (FaultReportStatus::Dispatched, "dispatched"),
            (FaultReportStatus::Fixed, "fixed"),
            (FaultReportStatus::Closed, "closed"),
        ];
        for (v, s) in fstatus {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let fevent = [
            (FaultReportEventType::Reported, "reported"),
            (FaultReportEventType::Dispatched, "dispatched"),
            (FaultReportEventType::Reassigned, "reassigned"),
            (FaultReportEventType::Fixed, "fixed"),
            (FaultReportEventType::Closed, "closed"),
            (
                FaultReportEventType::MigrationBaseline,
                "migration_baseline",
            ),
        ];
        for (v, s) in fevent {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let severity = [
            (AlertSeverity::Warning, "warning"),
            (AlertSeverity::Critical, "critical"),
            (AlertSeverity::Fatal, "fatal"),
        ];
        for (v, s) in severity {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let alert = [
            (AlertStatus::Active, "active"),
            (AlertStatus::Acknowledged, "acknowledged"),
            (AlertStatus::Resolved, "resolved"),
            (AlertStatus::AutoResolved, "auto_resolved"),
        ];
        for (v, s) in alert {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let import = [
            (DeviceImportStatus::Pending, "pending"),
            (DeviceImportStatus::Completed, "completed"),
            (DeviceImportStatus::Failed, "failed"),
        ];
        for (v, s) in import {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let meta = [
            (DeviceMetaStatus::Enabled, "enabled"),
            (DeviceMetaStatus::Disabled, "disabled"),
            (DeviceMetaStatus::Retired, "retired"),
            (DeviceMetaStatus::Fault, "fault"),
        ];
        for (v, s) in meta {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let dev = [
            (DeviceStatus::Enabled, "enabled"),
            (DeviceStatus::Disabled, "disabled"),
            (DeviceStatus::Retired, "retired"),
        ];
        for (v, s) in dev {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let port = [
            (DevicePortStatus::Idle, "idle"),
            (DevicePortStatus::Charging, "charging"),
            (DevicePortStatus::Full, "full"),
            (DevicePortStatus::Fault, "fault"),
            (DevicePortStatus::Disabled, "disabled"),
        ];
        for (v, s) in port {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let station = [
            (StationStatus::Active, "active"),
            (StationStatus::Disabled, "disabled"),
            (StationStatus::Construction, "construction"),
        ];
        for (v, s) in station {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let session = [
            (SessionProtocol::Tcp, "tcp"),
            (SessionProtocol::Mqtt, "mqtt"),
        ];
        for (v, s) in session {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let vendor_proto = [
            (VendorProtocol::Tcp, "tcp"),
            (VendorProtocol::Mqtt, "mqtt"),
            (VendorProtocol::Hybrid, "hybrid"),
        ];
        for (v, s) in vendor_proto {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let vendor = [
            (VendorStatus::Enabled, "enabled"),
            (VendorStatus::Disabled, "disabled"),
        ];
        for (v, s) in vendor {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
