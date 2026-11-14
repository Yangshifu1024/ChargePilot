//! 遥测类型与计量分段生成：按设备累计读数差值构建计费分段，不做时间分摊。
//! 语义基准：旧仓库 internal/domain/devices/telemetry.go 与
//! internal/domain/charging/measured_segments.go，逐行对齐。

use crate::charging_actual::MeterSegment;
use chrono::{DateTime, Duration, Utc};

/// 某端口的累计电量与瞬时功率读数（旧版 devices.Reading）。
/// 功率零值表示设备上报 0 W，不能当作缺测，也不能从电量反推功率。
#[derive(Debug, Clone)]
pub struct Reading {
    /// 端口号，从 1 开始。
    pub port: u8,
    /// 设备累计充电秒数，加上设备会话起点得到读数时刻。
    pub charged_seconds: u32,
    /// 累计电量，单位 mWh。必须能被 1000 整除才能换算成 Wh。
    pub charged_mwh: u32,
    /// 瞬时功率，单位 W（设备 0.1 W 上报值已换算）。
    pub power_w: u32,
}

/// 一组设备读数及服务端接收时间（旧版 devices.ReadingSample）。
#[derive(Debug, Clone)]
pub struct ReadingSample {
    pub device_id: String,
    pub received_at: DateTime<Utc>,
    pub readings: Vec<Reading>,
}

/// 设备上报的会话起止、总时长和总电量（旧版 devices.SessionSpan）。
#[derive(Debug, Clone, Default)]
pub struct SessionSpan {
    pub device_id: String,
    pub port: u8,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// 设备累计秒数；必须等于 ended_at 与 started_at 的时间差。
    pub charged_seconds: u32,
    /// 设备累计电量，单位 Wh，用于校验分段合计。
    pub energy_wh: u32,
}

/// 单次计费的最大心跳条数（旧版 maxSamples），与网关取数上限一致。
const MAX_SAMPLES: usize = 10080;

/// 设备时钟与充电开始时刻的最大偏差（旧版 clockSkewTolerance）。
const CLOCK_SKEW_TOLERANCE: Duration = Duration::seconds(5);

/// 相邻心跳间隔不超过该值时，才用前次功率描述该区间（旧版 powerJoinWindow）。
const POWER_JOIN_WINDOW: Duration = Duration::seconds(60);

/// 按设备累计读数生成计费分段，不用接收时间分摊电量（旧版 MeasuredSegments）。
/// start 来自中央启动回执，span.started_at 来自设备结束帧，两者允许存在时钟偏差。
/// 起充前容差内且没有新增电量的读数可跳过，仍保留其功率供后续区间使用。
/// 时长、时钟、精度、读数顺序或电量合计不符时返回 None（空），交由计量复核。
pub fn measured_segments(
    start: DateTime<Utc>,
    span: &SessionSpan,
    samples: &[ReadingSample],
) -> Option<Vec<MeterSegment>> {
    if span.ended_at < start
        || span.ended_at - start > Duration::days(7)
        || span.ended_at - span.started_at != Duration::seconds(span.charged_seconds as i64)
        || samples.len() > MAX_SAMPLES
    {
        return None;
    }
    let delta = start - span.started_at;
    if delta < -CLOCK_SKEW_TOLERANCE || delta > CLOCK_SKEW_TOLERANCE {
        return None;
    }
    let mut cursor = start;
    let mut previous = 0u32;
    let mut previous_power: Option<u32> = None;
    let mut segments: Vec<MeterSegment> = Vec::new();
    for sample in samples {
        if sample.device_id != span.device_id {
            continue;
        }
        for reading in &sample.readings {
            if reading.port != span.port {
                continue;
            }
            let at = span.started_at + Duration::seconds(reading.charged_seconds as i64);
            let lag = sample.received_at - at;
            if lag < -CLOCK_SKEW_TOLERANCE
                || lag > CLOCK_SKEW_TOLERANCE
                || reading.charged_seconds > span.charged_seconds
                || reading.charged_mwh % 1000 != 0
            {
                return None;
            }
            let wh = reading.charged_mwh / 1000;
            if wh < previous || wh > span.energy_wh {
                return None;
            }
            if at < cursor {
                // 只跳过起充前容差内、且没有新增电量的读数；不移动分段起点或累计电量。
                // 功率仍可供后续区间使用。超出容差、起充后时间回退或电量增加都须拒绝。
                if at >= start || wh != previous || at < start - CLOCK_SKEW_TOLERANCE {
                    return None;
                }
                previous_power = Some(reading.power_w);
                continue;
            }
            if at == cursor {
                if wh != previous {
                    return None;
                }
                previous_power = Some(reading.power_w);
                continue;
            }
            let mut segment = MeterSegment {
                started_at: cursor,
                ended_at: at,
                energy_wh: wh - previous,
                peak_w: 0,
                power_w: None,
            };
            if let Some(v) = previous_power
                && at - cursor <= POWER_JOIN_WINDOW
            {
                segment.power_w = Some(v);
                segment.peak_w = reading.power_w.max(v);
            }
            segments.push(segment);
            previous_power = Some(reading.power_w);
            cursor = at;
            previous = wh;
        }
    }
    if segments.is_empty() {
        return None;
    }
    if cursor == span.ended_at {
        if previous != span.energy_wh {
            return None;
        }
    } else {
        let mut segment = MeterSegment {
            started_at: cursor,
            ended_at: span.ended_at,
            energy_wh: span.energy_wh - previous,
            peak_w: 0,
            power_w: None,
        };
        if let Some(v) = previous_power
            && span.ended_at - cursor <= POWER_JOIN_WINDOW
        {
            segment.power_w = Some(v);
            segment.peak_w = v;
        }
        segments.push(segment);
    }
    Some(segments)
}
