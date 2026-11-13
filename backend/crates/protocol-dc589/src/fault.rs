//! 故障码语义（C0 帧）：把协议故障码映射为指标名与严重度。
//! 红线：0xAA/0xBB/0xCC 等码值只出现在本文件，运行时与告警域只见
//! "smoke"/"high_temperature" 等中性指标名。

/// 告警严重度（对齐 central 告警域词表；本适配器只产出 fatal 与 critical，
/// warning 由词表保留以覆盖列约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    /// 严重（如烟雾告警）。
    Fatal,
    /// 危急。
    Critical,
    /// 警告（保留取值，无生产写入路径）。
    Warning,
}

impl AlertSeverity {
    /// 持久化用字符串（alert_event.severity 列词表）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fatal => "fatal",
            Self::Critical => "critical",
            Self::Warning => "warning",
        }
    }
}

/// 故障语义：指标名、严重度与说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultMetric {
    /// 指标名（告警聚合键）。
    pub metric: String,
    /// 严重度。
    pub severity: AlertSeverity,
    /// 说明。
    pub note: String,
}

/// 按故障码返回指标名、严重程度和说明。
/// 端口 0xFF 表示整机故障；其余故障按端口分别记录。
#[must_use]
pub fn fault_metric(port: u8, fault_code: u8) -> FaultMetric {
    if port == 0xFF {
        // 整机级故障码：烟雾 0xBB、高温 0xAA、输入电压异常 0xCC。
        match fault_code {
            0xBB => FaultMetric {
                metric: "smoke".into(),
                severity: AlertSeverity::Fatal,
                note: "设备上报烟雾告警".into(),
            },
            0xAA => FaultMetric {
                metric: "high_temperature".into(),
                severity: AlertSeverity::Critical,
                note: "设备上报高温告警".into(),
            },
            code => FaultMetric {
                metric: "device_fault".into(),
                severity: AlertSeverity::Critical,
                note: format!("设备上报故障（0x{code:02X}）"),
            },
        }
    } else {
        FaultMetric {
            metric: format!("port_fault_{port}"),
            severity: AlertSeverity::Critical,
            note: format!("端口 {port} 上报故障（0x{fault_code:02X}）"),
        }
    }
}

/// 依据故障之后的最新心跳判断告警是否已恢复：
/// 设备级告警要求设备状态归零，端口告警要求端口状态不大于 2。
/// 没有端口状态的心跳仍视为未恢复。
#[must_use]
pub fn fault_recovered(metric: &str, device_status: u8, port_states: Option<&[u8]>) -> bool {
    let Some(states) = port_states else {
        return false;
    };
    if metric == "smoke" || metric == "high_temperature" || metric == "device_fault" {
        return device_status == 0;
    }
    let Some(port) = metric
        .strip_prefix("port_fault_")
        .and_then(|p| p.parse::<usize>().ok())
    else {
        return false;
    };
    if port < 1 || port > states.len() {
        return false;
    }
    states[port - 1] <= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_device_faults() {
        // 烟雾 → fatal + smoke 指标；高温 → critical。
        let smoke = fault_metric(0xFF, 0xBB);
        assert_eq!(smoke.metric, "smoke");
        assert_eq!(smoke.severity, AlertSeverity::Fatal);
        let hot = fault_metric(0xFF, 0xAA);
        assert_eq!(hot.metric, "high_temperature");
        assert_eq!(hot.severity, AlertSeverity::Critical);
        let other = fault_metric(0xFF, 0xCC);
        assert_eq!(other.metric, "device_fault");
    }

    #[test]
    fn port_fault_and_recovery() {
        let f = fault_metric(3, 0x04);
        assert_eq!(f.metric, "port_fault_3");
        // 端口 3 状态 4（粘连）未恢复；降为 2 恢复。
        assert!(!fault_recovered("port_fault_3", 0, Some(&[0, 0, 4, 0])));
        assert!(fault_recovered("port_fault_3", 0, Some(&[0, 0, 2, 0])));
        // 设备级恢复要求整机状态归零。
        assert!(!fault_recovered("smoke", 2, Some(&[0; 8])));
        assert!(fault_recovered("smoke", 0, Some(&[0; 8])));
        // 无端口状态心跳不判恢复。
        assert!(!fault_recovered("port_fault_1", 0, None));
    }
}
