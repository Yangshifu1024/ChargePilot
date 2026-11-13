//! 设备方向编解码：主板发送帧（A0/B4/B8/BA/BB/C2/C6/D0）与解析平台
//! 下发帧（A1/B7/B9/A6/E0）。服务端与模拟器复用同一字段布局，通过
//! 往返测试保证编码与解析一致。
//!
//! 板号校验比通用设备编号严格：固定 16 位十进制，按 8 字节 BCD。

use crate::{
    BcdError,
    commands::{ChargeMode, PayloadError},
    encode_bcd, encode_civil_time, encode_identifier,
};
use chargepilot_protocol::PortTelemetry;
use chrono::{DateTime, Utc};

/// 板号固定 16 位十进制长度（8 字节 BCD）。
pub const BOARD_ID_DIGITS: usize = 16;

/// 主板在 A0 中传递的固定设备信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// 主板编号（16 位十进制）。
    pub board_id: String,
    /// 硬件版本（8 字符）。
    pub hardware_version: String,
    /// 软件 ID（8 字符）。
    pub software_id: String,
    /// 软件版本。
    pub software_version: u16,
    /// 模块编号。
    pub module_id: String,
    /// SIM 卡号。
    pub sim: String,
    /// 信号值。
    pub signal: u8,
}

/// 编码 0xA0 注册帧；板号必须为 16 位十进制数字，按 BCD 表示，不截断。
pub fn build_registration(identity: &DeviceIdentity) -> Result<crate::frame::Frame, PayloadError> {
    let board = encode_bcd(&identity.board_id).map_err(|BcdError| PayloadError)?;
    if board.len() != 8 || identity.hardware_version.len() != 8 || identity.software_id.len() != 8 {
        return Err(PayloadError);
    }
    let module = encode_identifier(&identity.module_id, 8).map_err(|BcdError| PayloadError)?;
    let sim = encode_identifier(&identity.sim, 10).map_err(|BcdError| PayloadError)?;
    let mut data = vec![0u8; 45];
    data[..8].copy_from_slice(&board);
    data[8..16].copy_from_slice(identity.hardware_version.as_bytes());
    data[16..24].copy_from_slice(identity.software_id.as_bytes());
    data[24..26].copy_from_slice(&identity.software_version.to_le_bytes());
    data[26..34].copy_from_slice(&module);
    data[34..44].copy_from_slice(&sim);
    data[44] = identity.signal;
    Ok(crate::frame::Frame::new(0xA0, [0; 6], data))
}

/// 可选扩展心跳的整机状态；未上报扩展字段时使用 17 字节短格式。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortStatus {
    /// 整机状态位。
    pub device_status: u8,
    /// 输入电压（V）。
    pub voltage_v: u16,
    /// 温度（摄氏；编码时加 50 偏置）。
    pub temperature_c: i16,
    /// 各端口状态。
    pub port_states: Vec<u8>,
    /// 充电端口遥测。
    pub charging: Vec<PortTelemetry>,
}

/// 编码 A4 心跳；status=None 用 17 字节短格式，否则用 A6 启用的扩展格式。
pub fn build_heartbeat(
    identity: &DeviceIdentity,
    status: Option<&PortStatus>,
) -> Result<crate::frame::Frame, PayloadError> {
    let board = encode_bcd(&identity.board_id).map_err(|BcdError| PayloadError)?;
    let module = encode_identifier(&identity.module_id, 8).map_err(|BcdError| PayloadError)?;
    let mut data = vec![0u8; 17];
    data[..8].copy_from_slice(&board);
    data[8..16].copy_from_slice(&module);
    data[16] = identity.signal;
    let Some(status) = status else {
        return Ok(crate::frame::Frame::new(0xA4, [0; 6], data));
    };
    if status.port_states.is_empty() || status.port_states.len() > 64 {
        return Err(PayloadError);
    }
    // 载荷最大 247 字节，单帧不能包含超出该限制的全部端口状态。
    if 23 + status.port_states.len() + 13 * status.charging.len() > 255 {
        return Err(PayloadError);
    }
    let temp = i32::from(status.temperature_c) + 50;
    if !(0..=255).contains(&temp) {
        return Err(PayloadError);
    }
    data.push(status.device_status);
    data.extend_from_slice(&status.voltage_v.to_le_bytes());
    data.push(temp as u8);
    data.push(status.port_states.len() as u8);
    data.extend_from_slice(&status.port_states);
    data.push(status.charging.len() as u8);
    for port in &status.charging {
        if port.port == 0
            || port.port as usize > status.port_states.len()
            || port.remaining_secs % 60 > 59
            || port.charged_seconds % 60 > 59
        {
            return Err(PayloadError);
        }
        let mut entry = vec![0u8; 13];
        entry[0] = port.port;
        entry[1..3].copy_from_slice(((port.remaining_secs / 60) as u16).to_le_bytes().as_slice());
        entry[3] = (port.remaining_secs % 60) as u8;
        entry[4..6].copy_from_slice(
            ((port.charged_seconds / 60) as u16)
                .to_le_bytes()
                .as_slice(),
        );
        entry[6] = (port.charged_seconds % 60) as u8;
        // 协议电量单位为 Wh；内部 mWh 除以 1000 后写入帧。
        entry[7..9].copy_from_slice(&((port.remaining_mwh / 1000) as u16).to_le_bytes());
        entry[9..11].copy_from_slice(&((port.charged_mwh / 1000) as u16).to_le_bytes());
        entry[11..13].copy_from_slice(&(port.power_deci_watts as u16).to_le_bytes());
        data.extend_from_slice(&entry);
    }
    Ok(crate::frame::Frame::new(0xA4, [0; 6], data))
}

/// 编码 B8 启动应答或 BA 停止应答（共用两字节格式）。
pub fn build_command_result(
    command: u8,
    code: u8,
    port: u8,
) -> Result<crate::frame::Frame, PayloadError> {
    if command != 0xB8 && command != 0xBA {
        return Err(PayloadError);
    }
    Ok(crate::frame::Frame::new(command, [0; 6], vec![code, port]))
}

/// BB 结束上报内容：自行结束或被叫停。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeEndReport {
    /// 端口号。
    pub port: u8,
    /// 设备订单号（8 字节 BCD 原文，回显启动命令中的值）。
    pub order_bcd: [u8; 8],
    /// 设备侧开始时刻。
    pub started_at: DateTime<Utc>,
    /// 设备侧结束时刻。
    pub ended_at: DateTime<Utc>,
    /// 累计电量（mWh）。
    pub charged_mwh: u32,
    /// 结束瞬时功率（0.1 W）。
    pub power_deci_watts: u32,
    /// 停止原因码。
    pub stop_reason: u8,
    /// 消费类型码。
    pub consumer_type: u8,
}

/// 编码 BB 的 44 字节载荷：各字段按协议偏移，保留位填零。
pub fn build_charge_end(report: &ChargeEndReport) -> Result<crate::frame::Frame, PayloadError> {
    if report.port == 0 || report.ended_at < report.started_at {
        return Err(PayloadError);
    }
    let mut data = vec![0u8; 44];
    data[1] = report.port;
    data[2..10].copy_from_slice(&report.order_bcd);
    encode_civil_time(&mut data[10..16], report.started_at);
    encode_civil_time(&mut data[16..22], report.ended_at);
    // 时长拆分为分钟及 0-59 秒余数，满足服务端时长重建校验。
    let seconds = (report.ended_at - report.started_at).num_seconds().max(0) as u32;
    data[26..28].copy_from_slice(&((seconds / 60) as u16).to_le_bytes());
    data[28] = (seconds % 60) as u8;
    data[29] = report.consumer_type;
    data[32..34].copy_from_slice(&((report.charged_mwh / 1000) as u16).to_le_bytes());
    data[40] = report.stop_reason;
    data[42..44].copy_from_slice(&(report.power_deci_watts as u16).to_le_bytes());
    Ok(crate::frame::Frame::new(0xBB, [0; 6], data))
}

/// 编码 C2 分档上报：端口、剩余秒数与功率。
pub fn build_charging_band(
    port: u8,
    remaining_secs: u32,
    power_deci_watts: u32,
) -> Result<crate::frame::Frame, PayloadError> {
    if port == 0 {
        return Err(PayloadError);
    }
    let mut data = vec![0u8; 9];
    data[0] = port;
    data[4..6].copy_from_slice(&((remaining_secs / 60) as u16).to_le_bytes());
    data[7..9].copy_from_slice(&(power_deci_watts as u16).to_le_bytes());
    Ok(crate::frame::Frame::new(0xC2, [0; 6], data))
}

/// 编码 B4 本地启动上报（模拟器/测试用；平台侧解码见命令分派）。
pub fn build_local_start(report: &ChargeEndReport) -> Result<crate::frame::Frame, PayloadError> {
    if report.port == 0 {
        return Err(PayloadError);
    }
    let mut data = vec![0u8; 36];
    data[0] = report.port;
    data[1] = 1; // 充电中
    data[2..10].copy_from_slice(&report.order_bcd);
    encode_civil_time(&mut data[10..16], report.started_at);
    data[29] = report.consumer_type;
    data[30..32].copy_from_slice(&((report.charged_mwh / 1000) as u16).to_le_bytes());
    data[32..34].copy_from_slice(&(report.power_deci_watts as u16).to_le_bytes());
    Ok(crate::frame::Frame::new(0xB4, [0; 6], data))
}

/// 解析 A1 注册回复：返回联网状态与服务器时间（模拟器用）。
pub fn parse_register_reply(command: u8, data: &[u8]) -> Result<(u8, DateTime<Utc>), PayloadError> {
    crate::commands::parse_register_reply(command, data)
}

/// 解析 A6 心跳设置：返回周期与端口遥测开关（模拟器用）。
pub fn parse_heartbeat_interval(data: &[u8]) -> Result<(u16, bool), PayloadError> {
    if data.len() != 3 {
        return Err(PayloadError);
    }
    Ok((u16::from_le_bytes([data[0], data[1]]), data[2] == 1))
}

/// 解析 B7 启动命令（模拟器用）：提取模式、额度、端口及订单标识。
pub fn parse_start_command(
    data: &[u8],
) -> Result<(u8, [u8; 8], ChargeMode, u16, u8, u32, u16), PayloadError> {
    if data.len() != 19 || data[0] == 0 {
        return Err(PayloadError);
    }
    let mut order_bcd = [0u8; 8];
    order_bcd.copy_from_slice(&data[1..9]);
    let mode = ChargeMode::from_code(data[9]).ok_or(PayloadError)?;
    let consumer = data[10];
    let card = u32::from_le_bytes([data[13], data[14], data[15], data[16]]);
    let balance = u16::from_le_bytes([data[17], data[18]]);
    Ok((
        data[0],
        order_bcd,
        mode,
        u16::from_le_bytes([data[11], data[12]]),
        consumer,
        card,
        balance,
    ))
}

/// 解析 B9 停止命令：1 字节端口号（大于零）。
pub fn parse_stop_command(data: &[u8]) -> Result<u8, PayloadError> {
    if data.len() != 1 || data[0] == 0 {
        return Err(PayloadError);
    }
    Ok(data[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::china_offset;
    use chrono::TimeZone;

    fn identity() -> DeviceIdentity {
        DeviceIdentity {
            board_id: "5348240514082652".into(),
            hardware_version: "SH10HP01".into(),
            software_id: "SH10SP02".into(),
            software_version: 107,
            module_id: "0863121077410587".into(),
            sim: "898604041018C0013999".into(),
            signal: 17,
        }
    }

    #[test]
    fn registration_golden() {
        // 与协议文档 A0 示例帧逐字节对照（LEN 0x35，DATA 45 字节）。
        let frame = build_registration(&identity()).unwrap();
        let raw = crate::frame::encode(&frame).unwrap();
        assert_eq!(raw[1], 0x35);
        assert_eq!(raw[2], 0xA0);
        assert_eq!(
            &raw[9..9 + 8],
            &[0x53, 0x48, 0x24, 0x05, 0x14, 0x08, 0x26, 0x52]
        );
        // 平台侧解析回读一致。
        let reg = crate::commands::parse_registration(0xA0, &frame.data).unwrap();
        assert_eq!(reg.board_id, "5348240514082652");
        assert_eq!(reg.signal, 17);
    }

    #[test]
    fn board_id_must_be_sixteen_digits() {
        let mut id = identity();
        id.board_id = "12345".into();
        assert_eq!(build_registration(&id), Err(PayloadError));
        id.board_id = "123456789012345G".into();
        assert_eq!(build_registration(&id), Err(PayloadError));
    }

    #[test]
    fn heartbeat_roundtrip_with_telemetry() {
        let status = PortStatus {
            device_status: 0,
            voltage_v: 220,
            temperature_c: 24,
            port_states: vec![0, 1, 0, 0],
            charging: vec![PortTelemetry {
                port: 2,
                remaining_secs: 100 * 60,
                charged_seconds: 65,
                remaining_mwh: 500_000,
                charged_mwh: 69_000,
                power_deci_watts: 1295,
            }],
        };
        let frame = build_heartbeat(&identity(), Some(&status)).unwrap();
        let hb = crate::measurements::parse_heartbeat(&frame.data).unwrap();
        assert_eq!(hb.board_id, "5348240514082652");
        assert_eq!(hb.temperature_c, 24);
        assert_eq!(hb.charging_ports, status.charging);
        assert_eq!(hb.port_states, status.port_states);
    }

    #[test]
    fn charge_end_roundtrip() {
        let started = china_offset()
            .with_ymd_and_hms(2024, 8, 29, 15, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let ended = china_offset()
            .with_ymd_and_hms(2024, 8, 29, 20, 0, 30)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let report = ChargeEndReport {
            port: 5,
            order_bcd: [0x12, 0x34, 0, 0, 0, 0, 0, 0],
            started_at: started,
            ended_at: ended,
            charged_mwh: 1_000_000,
            power_deci_watts: 2000,
            stop_reason: 7,
            consumer_type: 2,
        };
        let frame = build_charge_end(&report).unwrap();
        let end = crate::measurements::parse_charge_end(&frame.data).unwrap();
        assert_eq!(end.port, 5);
        // 300 分 30 秒。
        assert_eq!(end.charged_seconds, 300 * 60 + 30);
        assert_eq!(end.charged_mwh, 1_000_000);
        assert_eq!(end.stop_reason, 7);
        assert_eq!(end.consumer_type, 2);
        assert_eq!(end.power_deci_watts, 2000);
    }

    #[test]
    fn parse_platform_commands() {
        // B7 启动：端口 5、平台计费、100 分钟、扫码。
        let start = crate::commands::StartCommand {
            consumer_type: 2,
            card_number: 0,
            card_balance_units: 0,
            session: [1, 2, 3, 4, 5, 6],
            port: 5,
            order_bcd: [0x11; 8],
            mode: ChargeMode::PlatformBilling,
            quantity: 100,
        };
        let frame = crate::commands::build_start(&start).unwrap();
        let (port, order, mode, quantity, consumer, card, balance) =
            parse_start_command(&frame.data).unwrap();
        assert_eq!(
            (port, mode, quantity, consumer, card, balance),
            (5, ChargeMode::PlatformBilling, 100, 2, 0, 0)
        );
        assert_eq!(order, [0x11; 8]);
        // B9 停止。
        let stop = crate::commands::build_stop([0; 6], 3).unwrap();
        assert_eq!(parse_stop_command(&stop.data).unwrap(), 3);
        // A6 心跳设置。
        let a6 = crate::commands::build_heartbeat_interval([0; 6], 15, true).unwrap();
        assert_eq!(parse_heartbeat_interval(&a6.data).unwrap(), (15, true));
        // A1 注册回复。
        let now = china_offset()
            .with_ymd_and_hms(2024, 3, 15, 9, 30, 30)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let a1 = crate::commands::build_register_reply([0; 6], now);
        let (status, at) = parse_register_reply(0xA1, &a1.data).unwrap();
        assert_eq!(
            (status, at.with_timezone(&china_offset()).to_rfc3339()),
            (0, at.with_timezone(&china_offset()).to_rfc3339())
        );
    }
}
