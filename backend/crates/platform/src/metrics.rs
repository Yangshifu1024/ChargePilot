//! 指标：Prometheus 导出器 + G6 指标清单注册（阶段 01 注册，后续阶段填充）。

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 安装全局 recorder 并返回 /metrics 端点用的句柄。进程内只调用一次。
pub fn install() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    register_gauges();
    Ok(handle)
}

/// 注册 G6 清单（阶段 01 先置零可用，见 docs/adr/005）：
/// 连接数 / 充电数 / 帧速率 / 批量缓冲深度与丢弃计数 / 事件投递延迟 / 兜底扫描滞后 / DB 批量大小。
fn register_gauges() {
    metrics::describe_gauge!("device_connections", "当前设备 TCP 连接数");
    metrics::describe_gauge!("device_charging", "当前充电中设备数");
    metrics::describe_counter!("device_frames_total", "设备帧接收总数");
    metrics::describe_gauge!("telemetry_buffer_depth", "遥测批量缓冲深度");
    metrics::describe_counter!("telemetry_buffer_dropped_total", "遥测明细因缓冲满被丢弃数");
    metrics::describe_histogram!(
        "event_dispatch_latency_seconds",
        metrics::Unit::Seconds,
        "事件投递延迟"
    );
    metrics::describe_gauge!("sweep_lag_seconds", metrics::Unit::Seconds, "兜底扫描滞后");
    metrics::describe_histogram!("db_batch_rows", "DB 批量写入行数");
    // 置零初始化，保证指标在无流量时也可见
    metrics::gauge!("device_connections").set(0.0);
    metrics::gauge!("device_charging").set(0.0);
    metrics::gauge!("telemetry_buffer_depth").set(0.0);
}

/// 便捷计数（供其他 crate 使用，避免到处写 metrics:: 宏路径）。
pub fn bump(name: &'static str) {
    metrics::counter!(name).increment(1);
}
