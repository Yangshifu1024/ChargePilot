//! 遥测指标词表与单位换算（架构评审候选 5 定案）：写侧投影与读侧过滤
//! 共用的真相。真相源锚点为 `migrations/0001_init.sql` 列注释，名单与
//! 全仓实际写入一一对应——新增指标改一处，编译器逼写读两侧同步。

/// 遥测指标词表（`telemetry.metric` 与聚合表同名列；列为无约束 VARCHAR，
/// 投影字符串不入 sqlx 三件套）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TelemetryMetric {
    /// 设备输入电压（V；设备级）。
    VoltageV,
    /// 设备温度（摄氏；设备级）。
    TemperatureC,
    /// 端口瞬时功率（W；端口级）。
    PowerW,
    /// 端口累计电量（kWh；端口级）。
    MeterKwh,
}

impl TelemetryMetric {
    /// DB 字面量锚点（与列注释词表逐字节一致）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VoltageV => "voltage_v",
            Self::TemperatureC => "temperature_c",
            Self::PowerW => "power_w",
            Self::MeterKwh => "meter_kwh",
        }
    }

    /// 字面量 → 指标；未知值 None。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "voltage_v" => Self::VoltageV,
            "temperature_c" => Self::TemperatureC,
            "power_w" => Self::PowerW,
            "meter_kwh" => Self::MeterKwh,
            _ => return None,
        })
    }

    /// 设备级指标（port_no 为 NULL 的行）。
    #[must_use]
    pub const fn device_level() -> &'static [Self] {
        &[Self::VoltageV, Self::TemperatureC]
    }

    /// 全部成员（读侧展示键与测试用）。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::VoltageV,
            Self::TemperatureC,
            Self::PowerW,
            Self::MeterKwh,
        ]
    }
}

/// 帧单位 0.1 W → W（入库与展示单位）；单位语义的唯一出处
/// （帧字段单位契约见 `protocol::PortTelemetry` 注释）。
#[must_use]
pub fn watts_from_deci_watts(deci_watts: u32) -> f64 {
    f64::from(deci_watts) / 10.0
}

/// 帧单位 mWh → kWh（入库与展示单位）；单位语义的唯一出处。
#[must_use]
pub fn kwh_from_mwh(mwh: u32) -> f64 {
    f64::from(mwh) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 词表与换算函数的行为锁定（真相源：migrations 列注释）。
    #[test]
    fn telemetry_metric_roundtrip_and_units() {
        for metric in TelemetryMetric::all() {
            assert_eq!(TelemetryMetric::parse(metric.as_str()), Some(*metric));
        }
        // 设计期的愿望值（设备帧无电流/电池电量数据源）不在词表。
        assert_eq!(TelemetryMetric::parse("current_a"), None);
        assert_eq!(TelemetryMetric::parse("battery_soc"), None);
        assert_eq!(TelemetryMetric::device_level().len(), 2);
        // 单位换算锚点：0.1 W 与 mWh 的十进制映射。
        assert_eq!(watts_from_deci_watts(123), 12.3);
        assert_eq!(kwh_from_mwh(1_500_000), 1.5);
        assert_eq!(kwh_from_mwh(0), 0.0);
    }
}
