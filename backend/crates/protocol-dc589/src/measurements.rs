//! 计量帧编解码：心跳（A4）、结束订单（BB）与充电分档（C2）。
//! 电量单位换算在本模块完成：协议为 Wh（0.001 kWh），内部统一 mWh。

use crate::{commands::PayloadError, decode_bcd, decode_civil_time, decode_identifier};
use chargepilot_protocol::{Event, PortTelemetry};
use chrono::{DateTime, Utc};

/// 心跳数据：公共字段 + A6 开启时可能不完整的充电端口集合。
/// 端口超过 12 个的主板可能发两个 A4 帧，每个只带 10 条充电端口测量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatData {
    /// 主板编号。
    pub board_id: String,
    /// 模块编号。
    pub module_id: String,
    /// 信号值。
    pub signal: u8,
    /// 是否附带端口状态（A6 开关）。
    pub has_port_status: bool,
    /// 整机状态位（位 0 高温、位 1 烟雾、位 2 输入电压异常）。
    pub device_status: u8,
    /// 输入电压（V）。
    pub voltage_v: u16,
    /// 温度（摄氏；协议原值偏移 +50，解码时还原）。
    pub temperature_c: i16,
    /// 各端口状态（0 空闲 / 1 充电 / 3 输出故障 / 4 粘连）。
    pub port_states: Vec<u8>,
    /// 充电端口遥测（仅充电端口附带）。
    pub charging_ports: Vec<PortTelemetry>,
}

const HEARTBEAT_MIN: usize = 17;

/// 解码 A4 短心跳或带端口状态的扩展心跳。
pub fn parse_heartbeat(data: &[u8]) -> Result<HeartbeatData, PayloadError> {
    if data.len() < HEARTBEAT_MIN {
        return Err(PayloadError);
    }
    let board = decode_bcd(&data[..8])?;
    let mut result = HeartbeatData {
        board_id: board,
        module_id: decode_identifier(&data[8..16]),
        signal: data[16],
        has_port_status: false,
        device_status: 0,
        voltage_v: 0,
        temperature_c: 0,
        port_states: Vec::new(),
        charging_ports: Vec::new(),
    };
    if data.len() == HEARTBEAT_MIN {
        return Ok(result);
    }
    if data.len() < 22 {
        return Err(PayloadError);
    }
    result.has_port_status = true;
    result.device_status = data[17];
    result.voltage_v = u16::from_le_bytes([data[18], data[19]]);
    result.temperature_c = i16::from(data[20]) - 50;
    let port_count = data[21] as usize;
    if port_count == 0 || port_count > 64 || data.len() < 23 + port_count {
        return Err(PayloadError);
    }
    result.port_states = data[22..22 + port_count].to_vec();
    let charged_total = data[22 + port_count] as usize;
    let mut remaining = &data[23 + port_count..];
    if remaining.len() % 13 != 0
        || remaining.len() / 13 > charged_total
        || charged_total > port_count
    {
        return Err(PayloadError);
    }
    let mut seen = std::collections::HashSet::new();
    while !remaining.is_empty() {
        let port = remaining[0];
        if port == 0
            || port as usize > port_count
            || !seen.insert(port)
            || remaining[3] > 59
            || remaining[6] > 59
        {
            return Err(PayloadError);
        }
        result.charging_ports.push(PortTelemetry {
            port,
            remaining_secs: u32::from(u16::from_le_bytes([remaining[1], remaining[2]])) * 60
                + u32::from(remaining[3]),
            charged_seconds: u32::from(u16::from_le_bytes([remaining[4], remaining[5]])) * 60
                + u32::from(remaining[6]),
            remaining_mwh: u32::from(u16::from_le_bytes([remaining[7], remaining[8]])) * 1000,
            charged_mwh: u32::from(u16::from_le_bytes([remaining[9], remaining[10]])) * 1000,
            power_deci_watts: u32::from(u16::from_le_bytes([remaining[11], remaining[12]])),
        });
        remaining = &remaining[13..];
    }
    Ok(result)
}

/// 把心跳数据装配为协议无关事件（电量为 Wh：mWh/1000 与旧版事件键一致）。
#[must_use]
pub fn heartbeat_event(
    device_id: &str,
    heartbeat: &HeartbeatData,
    received_at: DateTime<Utc>,
    raw: Vec<u8>,
    session: [u8; 6],
) -> Event {
    Event {
        device_id: device_id.to_string(),
        kind: Some(chargepilot_protocol::EventType::Heartbeat),
        received_at: Some(received_at),
        raw_payload: raw,
        session_id: session,
        signal: heartbeat.signal,
        device_status: heartbeat.device_status,
        voltage_v: heartbeat.voltage_v,
        temperature_c: heartbeat.temperature_c,
        port_states: heartbeat.port_states.clone(),
        charging_ports: heartbeat.charging_ports.clone(),
        ..Event::default()
    }
}

/// BB 结束订单数据：设备上报的结束时间、用电量和扣费金额。
/// 平台结算按订单计费规则计算，不直接采用设备上报金额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeEndData {
    /// 端口号。
    pub port: u8,
    /// 设备订单号（BCD 解码）。
    pub order_number: String,
    /// 设备侧开始时刻。
    pub started_at: DateTime<Utc>,
    /// 设备侧结束时刻。
    pub ended_at: DateTime<Utc>,
    /// 充电类型码。
    pub charge_type: u8,
    /// 累计充电秒数。
    pub charged_seconds: u32,
    /// 上报时剩余的授权秒数（分档折扣由固件还原后上报）。
    pub remaining_seconds: u32,
    /// 累计电量（mWh）。
    pub charged_mwh: u32,
    /// 设备上报的诊断金额（分；BB 单位 0.01 元，仅诊断用）。
    pub amount_cents: i64,
    /// 停止原因码（设备词表）。
    pub stop_reason: u8,
    /// 消费类型码。
    pub consumer_type: u8,
    /// 结束瞬时功率（0.1 W）。
    pub power_deci_watts: u32,
    /// 结束时的功率档位（1..5）。
    pub band: u8,
}

/// 0xBB 载荷 44 字节。
pub fn parse_charge_end(data: &[u8]) -> Result<ChargeEndData, PayloadError> {
    if data.len() != 44 || data[1] == 0 {
        return Err(PayloadError);
    }
    let order = decode_bcd(&data[2..10])?;
    let started = decode_civil_time(&data[10..16])?;
    let ended = decode_civil_time(&data[16..22])?;
    if ended < started {
        return Err(PayloadError);
    }
    if data[28] > 59 || data[24] > 59 {
        return Err(PayloadError);
    }
    let band = data[41];
    if band > 5 {
        return Err(PayloadError);
    }
    Ok(ChargeEndData {
        port: data[1],
        order_number: order,
        started_at: started,
        ended_at: ended,
        charge_type: data[25],
        remaining_seconds: u32::from(u16::from_le_bytes([data[22], data[23]])) * 60
            + u32::from(data[24]),
        charged_seconds: u32::from(u16::from_le_bytes([data[26], data[27]])) * 60
            + u32::from(data[28]),
        consumer_type: data[29],
        charged_mwh: u32::from(u16::from_le_bytes([data[32], data[33]])) * 1000,
        amount_cents: i64::from(u16::from_le_bytes([data[34], data[35]])),
        stop_reason: data[40],
        band,
        power_deci_watts: u32::from(u16::from_le_bytes([data[42], data[43]])),
    })
}

/// 把结束订单装配为协议无关事件（电量为 Wh：mWh/1000 与旧版事件键一致）。
#[must_use]
pub fn charge_end_event(
    device_id: &str,
    end: &ChargeEndData,
    received_at: DateTime<Utc>,
    raw: Vec<u8>,
    session: [u8; 6],
) -> Event {
    Event {
        device_id: device_id.to_string(),
        kind: Some(chargepilot_protocol::EventType::ChargeEnd),
        port: end.port,
        received_at: Some(received_at),
        raw_payload: raw,
        session_id: session,
        energy_wh: end.charged_mwh / 1000,
        power_deci_watts: end.power_deci_watts,
        stop_reason: end.stop_reason,
        consumer_type: end.consumer_type,
        order_number: end.order_number.clone(),
        charged_seconds: end.charged_seconds,
        started_at: Some(end.started_at),
        ended_at: Some(end.ended_at),
        amount_cents: end.amount_cents,
        ..Event::default()
    }
}

/// 解码 C2 档位变化上报：返回端口、剩余秒数与功率（无需应答）。
pub fn parse_charging_band(data: &[u8]) -> Result<PortTelemetry, PayloadError> {
    if data.len() != 9 || data[0] == 0 {
        return Err(PayloadError);
    }
    Ok(PortTelemetry {
        port: data[0],
        remaining_secs: u32::from(u16::from_le_bytes([data[4], data[5]])) * 60,
        power_deci_watts: u32::from(u16::from_le_bytes([data[7], data[8]])),
        ..PortTelemetry::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议文档 A4 示例的 DATA 段（56 字节，原文校验合法）：
    /// 20 端口、15 号充电，剩余 206 分 52 秒、已充 33 分 8 秒、
    /// 已充 0.069 kWh、瞬时功率 129.5 W。
    const A4_GOLDEN: &[u8] = &[
        0x53, 0x48, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, // 板号
        0x08, 0x64, 0x60, 0x60, 0x66, 0x44, 0x95, 0x79, // 模块号
        0x15, // 信号 21
        0x00, // 整机正常
        0x95, 0x00, // 电压 149 V
        0x4A, // 温度 74-50=24
        0x14, // 20 端口
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, // 端口状态（15 号=1）
        0x01, // 充电端口总数
        0x0F, 0xCE, 0x00, 0x34, 0x21, 0x00, 0x08, 0x00, 0x00, 0x45, 0x00, 0x0F,
        0x05, // 计量块
    ];

    #[test]
    fn heartbeat_golden_twenty_ports() {
        let hb = parse_heartbeat(A4_GOLDEN).unwrap();
        assert_eq!(hb.board_id, "5348111111111111");
        assert_eq!(hb.module_id, "0864606066449579");
        assert_eq!(hb.signal, 21);
        assert!(hb.has_port_status);
        assert_eq!(hb.device_status, 0);
        assert_eq!(hb.voltage_v, 149);
        // 温度按偏移定义还原：0x4A-50 = 24（原文注释 74℃ 与定义不一致，按字段表）。
        assert_eq!(hb.temperature_c, 24);
        assert_eq!(hb.port_states.len(), 20);
        assert_eq!(hb.port_states[14], 1, "15 号端口充电");
        assert_eq!(hb.charging_ports.len(), 1);
        let port = &hb.charging_ports[0];
        assert_eq!(port.port, 15);
        assert_eq!(port.remaining_secs, 206 * 60 + 52);
        // 已充分钟字节 0x21=33（文档注释"21 分"按十进制误读，按字段表 33 分 8 秒）。
        assert_eq!(port.charged_seconds, 33 * 60 + 8);
        assert_eq!(port.remaining_mwh, 0);
        assert_eq!(port.charged_mwh, 69_000, "0.069 kWh = 69 Wh = 69000 mWh");
        assert_eq!(port.power_deci_watts, 1295, "129.5 W");
    }

    #[test]
    fn heartbeat_short_form() {
        // 17 字节短心跳（A6 关闭端口状态）。
        let data: Vec<u8> = (0..8)
            .collect::<Vec<u8>>()
            .into_iter()
            .chain(std::iter::repeat_n(0u8, 8))
            .chain(std::iter::once(30u8))
            .collect();
        let hb = parse_heartbeat(&data).unwrap();
        assert!(!hb.has_port_status);
        assert_eq!(hb.signal, 30);
    }

    #[test]
    fn heartbeat_rejects_malformed() {
        // 计量块数量超出声明总数。
        let mut data = A4_GOLDEN.to_vec();
        data[22 + 20] = 0; // 声明 0 个充电端口但带计量块
        assert_eq!(parse_heartbeat(&data), Err(PayloadError));
        // 秒数越界。
        let mut data = A4_GOLDEN.to_vec();
        let base = 23 + 20;
        data[base + 3] = 60;
        assert_eq!(parse_heartbeat(&data), Err(PayloadError));
    }

    #[test]
    fn charge_end_golden() {
        // 按协议文档 BB 示例字段值程序化构造 44 字节载荷：
        // 端口 5、订单全零、2024-08-29 15:00:00→20:00:00、时间计费、
        // 已充 4620 分（原文 0C 12 小端）、扫码、已充 1 kWh、金额 1 元、
        // 时间用完停止、第一档、结束功率 200 W。
        let mut data = vec![0u8; 44];
        data[0] = 1;
        data[1] = 5;
        data[10..16].copy_from_slice(&[0x24, 0x08, 0x29, 0x15, 0x00, 0x00]);
        data[16..22].copy_from_slice(&[0x24, 0x08, 0x29, 0x20, 0x00, 0x00]);
        data[26..28].copy_from_slice(&4620u16.to_le_bytes());
        data[29] = 2;
        data[32..34].copy_from_slice(&1000u16.to_le_bytes());
        data[34..36].copy_from_slice(&100u16.to_le_bytes());
        data[40] = 0;
        data[41] = 1;
        data[42..44].copy_from_slice(&2000u16.to_le_bytes());
        let end = parse_charge_end(&data).unwrap();
        assert_eq!(end.port, 5);
        assert_eq!(end.order_number, "0000000000000000");
        let started = end.started_at.with_timezone(&crate::china_offset());
        let ended = end.ended_at.with_timezone(&crate::china_offset());
        use chrono::Datelike as _;
        use chrono::Timelike as _;
        assert_eq!(
            (
                started.year(),
                started.month(),
                started.day(),
                started.hour()
            ),
            (2024, 8, 29, 15)
        );
        assert_eq!((ended.hour(), ended.minute()), (20, 0));
        // 已充时长：0x120C=4620 分钟 + 0 秒。原文注释 300 分钟与字节不一致，
        // 按字段表小端解释（差异见协议文档末节）。
        assert_eq!(end.charged_seconds, 4620 * 60);
        assert_eq!(end.charged_mwh, 1_000_000, "1 kWh");
        assert_eq!(end.amount_cents, 100, "1 元 = 100 分（诊断值）");
        assert_eq!(end.stop_reason, 0, "时间用完");
        assert_eq!(end.band, 1);
        assert_eq!(end.power_deci_watts, 2000, "200 W");
    }

    #[test]
    fn charge_end_rejects() {
        let mut data = vec![0u8; 44];
        data[1] = 0; // 端口 0 拒绝
        assert_eq!(parse_charge_end(&data), Err(PayloadError));
        data[1] = 5;
        data[41] = 6; // 档位越界
        assert_eq!(parse_charge_end(&data), Err(PayloadError));
    }

    #[test]
    fn charging_band_golden() {
        // 协议文档 C2 示例：端口 1，100 分钟一档 → 90 分钟二档，功率字段按小端。
        let data = [0x01, 0x64, 0x00, 0x01, 0x5A, 0x00, 0x02, 0xD0, 0x7D];
        let meter = parse_charging_band(&data).unwrap();
        assert_eq!(meter.port, 1);
        assert_eq!(meter.remaining_secs, 90 * 60);
        assert_eq!(meter.power_deci_watts, 0x7DD0, "按字段表小端取值");
    }
}
