//! 平台侧指令编解码：注册（A0/A1）、心跳设置（A6/A7）、校时（A9）、
//! 启停命令（B7/B8/B9/BA）、结束与故障确认（BC/C1）、复位（A2）。
//! 指令码常量仅在本 crate 内使用，对外暴露中性结构。

use crate::{BcdError, decode_bcd, decode_civil_time, encode_civil_time};
use chrono::{DateTime, Utc};

/// 载荷错误：命令类型、载荷长度或字段值无效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadError;

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "无效的 5.8.9 载荷")
    }
}

impl std::error::Error for PayloadError {}

impl From<BcdError> for PayloadError {
    fn from(_: BcdError) -> Self {
        Self
    }
}

/// 充电模式：决定启动额度按时间还是电量解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChargeMode {
    /// 按时间（分钟）。
    ByTime = 0,
    /// 按电量（0.001 kWh）。
    ByEnergy = 1,
    /// 平台计费：分钟限定充电时间，费用由平台计算，主板不分档。
    PlatformBilling = 4,
}

impl ChargeMode {
    /// 从协议码值解析；拒绝硬件未实现的 2、3 与长时模式 10/11/12
    /// （长时变体绕过时间与电量上限，不允许通过普通启动入口）。
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::ByTime),
            1 => Some(Self::ByEnergy),
            4 => Some(Self::PlatformBilling),
            _ => None,
        }
    }

    /// 协议码值。
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// A0 注册帧：板号、固件版本、模块与 SIM 信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// 主板编号（16 位十进制）。
    pub board_id: String,
    /// 硬件版本（8 字符 ASCII）。
    pub hardware_version: String,
    /// 软件 ID（8 字符 ASCII）。
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

/// 解码 45 字节 A0 注册载荷；板号必须是十进制 BCD。
pub fn parse_registration(command: u8, data: &[u8]) -> Result<Registration, PayloadError> {
    // A0 指令码 0xA0；载荷固定 45 字节。
    if command != 0xA0 || data.len() != 45 {
        return Err(PayloadError);
    }
    let board = decode_bcd(&data[..8])?;
    Ok(Registration {
        board_id: board,
        hardware_version: String::from_utf8_lossy(&data[8..16]).into_owned(),
        software_id: String::from_utf8_lossy(&data[16..24]).into_owned(),
        software_version: u16::from_le_bytes([data[24], data[25]]),
        module_id: crate::decode_identifier(&data[26..34]),
        sim: crate::decode_identifier(&data[34..44]),
        signal: data[44],
    })
}

/// 编码 A1 注册成功回复：确认联网并下发时间供主板校时。
/// 注册与校时响应使用相同的协议民用时区编码，避免宿主机时区偏移。
pub fn build_register_reply(session: [u8; 6], now: DateTime<Utc>) -> crate::frame::Frame {
    let mut data = vec![0u8; 7];
    data[0] = 0; // 已联网
    encode_civil_time(&mut data[1..], now);
    crate::frame::Frame::new(0xA1, session, data)
}

/// 编码 A5 心跳确认回复（0x01 = 收到）。
pub fn build_heartbeat_reply(session: [u8; 6]) -> crate::frame::Frame {
    crate::frame::Frame::new(0xA5, session, vec![1])
}

/// 编码 A9 校时应答（平台时间按民用时区写入六字节 BCD）。
pub fn build_time_reply(session: [u8; 6], now: DateTime<Utc>) -> crate::frame::Frame {
    let mut data = vec![0u8; 6];
    encode_civil_time(&mut data, now);
    crate::frame::Frame::new(0xA9, session, data)
}

/// A6 心跳设置：周期与端口遥测开关（5.8.6 起两者均由平台下发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatSetting {
    /// 心跳周期（秒）。
    pub seconds: u16,
    /// 是否附带端口状态。
    pub port_status: bool,
}

/// 编码 A6 心跳周期与端口遥测开关；周期必须大于零。
pub fn build_heartbeat_interval(
    session: [u8; 6],
    seconds: u16,
    port_status: bool,
) -> Result<crate::frame::Frame, PayloadError> {
    if seconds == 0 {
        return Err(PayloadError);
    }
    Ok(crate::frame::Frame::new(
        0xA6,
        session,
        vec![seconds as u8, (seconds >> 8) as u8, u8::from(port_status)],
    ))
}

/// 解码 A7 心跳设置应答；true = 设备接受。
pub fn parse_heartbeat_set_reply(command: u8, data: &[u8]) -> Result<bool, PayloadError> {
    if command != 0xA7 || data.len() != 1 {
        return Err(PayloadError);
    }
    Ok(data[0] == 0)
}

/// B7 启动命令：端口、订单、模式、额度与卡信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCommand {
    /// 消费类型码（2 扫码 / 3 在线卡）。
    pub consumer_type: u8,
    /// 在线卡卡号（非在线卡为 0）。
    pub card_number: u32,
    /// 卡余额显示值（0.1 元）。
    pub card_balance_units: u16,
    /// 帧会话号。
    pub session: [u8; 6],
    /// 端口号（从 1 开始）。
    pub port: u8,
    /// 设备订单号（8 字节 BCD 原文）。
    pub order_bcd: [u8; 8],
    /// 充电模式。
    pub mode: ChargeMode,
    /// 启动额度（依模式为分钟或 0.001 kWh）。
    pub quantity: u16,
}

/// 编码扫码类型 2 或在线卡类型 3 的启动命令；卡模式包含卡号和余额。
/// 调用方必须先完成支付授权；长时变体不允许通过普通启动入口。
pub fn build_start(command: &StartCommand) -> Result<crate::frame::Frame, PayloadError> {
    if command.port == 0 || command.quantity == 0 {
        return Err(PayloadError);
    }
    let mut data = vec![0u8; 19];
    data[0] = command.port;
    data[1..9].copy_from_slice(&command.order_bcd);
    data[9] = command.mode.code();
    data[10] = 2; // 扫码
    if command.consumer_type != 0 && command.consumer_type != 2 && command.consumer_type != 3 {
        return Err(PayloadError);
    }
    if command.consumer_type == 3 {
        if command.card_number == 0 {
            return Err(PayloadError);
        }
        data[10] = 3; // 在线卡
        data[13..17].copy_from_slice(&command.card_number.to_le_bytes());
        data[17..19].copy_from_slice(&command.card_balance_units.to_le_bytes());
    } else if command.card_number != 0 || command.card_balance_units != 0 {
        return Err(PayloadError);
    }
    data[11..13].copy_from_slice(&command.quantity.to_le_bytes());
    Ok(crate::frame::Frame::new(0xB7, command.session, data))
}

/// 编码 B9 停止命令；端口号必须大于零。
pub fn build_stop(session: [u8; 6], port: u8) -> Result<crate::frame::Frame, PayloadError> {
    if port == 0 {
        return Err(PayloadError);
    }
    Ok(crate::frame::Frame::new(0xB9, session, vec![port]))
}

/// 编码 A2 主板复位命令，其余控制字段填零。
pub fn build_reboot(session: [u8; 6]) -> crate::frame::Frame {
    let mut data = vec![0u8; 10];
    data[0] = 1; // 复位
    crate::frame::Frame::new(0xA2, session, data)
}

/// 启动或停止回复中的结果码和端口号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandResult {
    /// 设备结果码原值。
    pub code: u8,
    /// 端口号。
    pub port: u8,
}

/// 解码 B8 或 BA 的两字节回复，保留设备结果码供调用方判断。
pub fn parse_command_result(command: u8, data: &[u8]) -> Result<CommandResult, PayloadError> {
    if (command != 0xB8 && command != 0xBA) || data.len() != 2 {
        return Err(PayloadError);
    }
    Ok(CommandResult {
        code: data[0],
        port: data[1],
    })
}

/// 编码 BC 结束确认（0 = 收到指定端口）。
pub fn build_charge_end_reply(session: [u8; 6], port: u8) -> crate::frame::Frame {
    crate::frame::Frame::new(0xBC, session, vec![0, port])
}

/// 编码 C1 故障确认（0 = 收到；端口 0xFF 表示整机）。
pub fn build_fault_reply(session: [u8; 6], port: u8) -> crate::frame::Frame {
    crate::frame::Frame::new(0xC1, session, vec![0, port])
}

/// 解析 A1 注册回复（设备侧/模拟器用）：返回联网状态与服务器时间。
pub fn parse_register_reply(command: u8, data: &[u8]) -> Result<(u8, DateTime<Utc>), PayloadError> {
    if command != 0xA1 || data.len() != 7 {
        return Err(PayloadError);
    }
    let at = decode_civil_time(&data[1..7])?;
    Ok((data[0], at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::china_offset;
    use chrono::TimeZone;

    #[test]
    fn registration_roundtrip() {
        // 协议文档 A0 示例：主板 5348240514082652、硬件 SH10HP01、
        // 软件 SH10SP02、版本 107、模块 863121077410587、SIM 含 C 半字节。
        let doc: Vec<u8> = vec![
            0x53, 0x48, 0x24, 0x05, 0x14, 0x08, 0x26, 0x52, 0x53, 0x48, 0x31, 0x30, 0x48, 0x50,
            0x30, 0x31, 0x53, 0x48, 0x31, 0x30, 0x53, 0x50, 0x30, 0x32, 0x6B, 0x00, 0x08, 0x63,
            0x12, 0x10, 0x77, 0x41, 0x05, 0x87, 0x89, 0x86, 0x04, 0x04, 0x10, 0x18, 0xC0, 0x01,
            0x39, 0x99, 0x11,
        ];
        let reg = parse_registration(0xA0, &doc).unwrap();
        assert_eq!(reg.board_id, "5348240514082652");
        assert_eq!(reg.hardware_version, "SH10HP01");
        assert_eq!(reg.software_id, "SH10SP02");
        assert_eq!(reg.software_version, 107);
        assert_eq!(reg.module_id, "0863121077410587");
        // SIM 含非十进制半字节 C：按大写十六进制兼容解码。
        assert_eq!(reg.sim, "898604041018C0013999");
        assert_eq!(reg.signal, 17);
    }

    #[test]
    fn register_reply_golden() {
        // 协议文档 A1 示例：已联网，2024-03-15 09:30:30。
        let now = china_offset()
            .with_ymd_and_hms(2024, 3, 15, 9, 30, 30)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let frame = build_register_reply([0x00, 0x00, 0x65, 0xF3, 0xB4, 0xD1], now);
        assert_eq!(
            crate::frame::encode(&frame).unwrap(),
            vec![
                0xEE, 0x0F, 0xA1, 0x00, 0x00, 0x65, 0xF3, 0xB4, 0xD1, 0x00, 0x24, 0x03, 0x15, 0x09,
                0x30, 0x30, 0x66
            ]
        );
    }

    #[test]
    fn heartbeat_interval_golden() {
        // 协议文档 A6 示例：60 秒、关闭端口状态上传。
        let frame = build_heartbeat_interval([0; 6], 60, false).unwrap();
        assert_eq!(
            crate::frame::encode(&frame).unwrap(),
            vec![0xEE, 0x0B, 0xA6, 0, 0, 0, 0, 0, 0, 0x3C, 0x00, 0x00, 0x91]
        );
    }

    #[test]
    fn start_command_scan_and_card() {
        // 文档 B7 示例：端口 5、订单全零、按时间、在线扫码、100 分钟、卡与余额为零。
        let scan = StartCommand {
            consumer_type: 2,
            card_number: 0,
            card_balance_units: 0,
            session: [0; 6],
            port: 5,
            order_bcd: [0; 8],
            mode: ChargeMode::ByTime,
            quantity: 100,
        };
        let frame = build_start(&scan).unwrap();
        // 按文档示例程序化构造整帧（余额按字段表补足 2 字节；
        // 文档原文余额仅 1 字节、LEN 0x1B 与实际长度存在末节差异，SUM 0xCF）。
        let mut doc = vec![0u8; 29];
        doc[0] = 0xEE;
        doc[1] = 0x1B;
        doc[2] = 0xB7;
        doc[9] = 5; // 端口
        doc[18] = 0; // 按时间
        doc[19] = 2; // 在线扫码
        doc[20..22].copy_from_slice(&100u16.to_le_bytes()); // 100 分钟
        doc[28] = 0xCF; // 文档 SUM（与本帧 LEN/内容一致）
        assert_eq!(crate::frame::encode(&frame).unwrap(), doc);
        // 在线卡启动：卡号与余额写入 13..19（卡号小端 17564161）。
        let card = StartCommand {
            consumer_type: 3,
            card_number: 17564161,
            card_balance_units: 10,
            session: [0; 6],
            port: 2,
            order_bcd: [0; 8],
            mode: ChargeMode::ByTime,
            quantity: 60,
        };
        let frame = build_start(&card).unwrap();
        let raw = crate::frame::encode(&frame).unwrap();
        assert_eq!(raw[9 + 10], 3);
        assert_eq!(&raw[9 + 13..9 + 17], &17564161u32.to_le_bytes());
        assert_eq!(&raw[9 + 17..9 + 19], &10u16.to_le_bytes());
    }

    #[test]
    fn start_rejects_invalid() {
        // 端口 0 / 额度 0 / 长时模式 / 扫码带卡号 均拒绝。
        let base = StartCommand {
            consumer_type: 2,
            card_number: 0,
            card_balance_units: 0,
            session: [0; 6],
            port: 1,
            order_bcd: [0; 8],
            mode: ChargeMode::ByTime,
            quantity: 60,
        };
        assert_eq!(
            build_start(&StartCommand {
                port: 0,
                ..base.clone()
            }),
            Err(PayloadError)
        );
        assert_eq!(
            build_start(&StartCommand {
                quantity: 0,
                ..base.clone()
            }),
            Err(PayloadError)
        );
        assert_eq!(
            build_start(&StartCommand {
                card_number: 5,
                ..base.clone()
            }),
            Err(PayloadError)
        );
        assert_eq!(build_stop([0; 6], 0), Err(PayloadError));
    }

    #[test]
    fn command_result_both_replies() {
        // B8/BA 共用两字节格式：文档 B8 示例 code=0、端口 5。
        let frame = crate::frame::Frame::new(0xB8, [0; 6], vec![0, 5]);
        let result = parse_command_result(0xB8, &frame.data).unwrap();
        assert_eq!(result, CommandResult { code: 0, port: 5 });
        // BA 停止成功码原表写 10（示例字节 0x10）。
        let stop = crate::frame::Frame::new(0xBA, [0; 6], vec![0x10, 5]);
        assert_eq!(parse_command_result(0xBA, &stop.data).unwrap().code, 0x10);
    }

    #[test]
    fn civil_time_roundtrip_and_reject() {
        let at = china_offset()
            .with_ymd_and_hms(2024, 8, 29, 10, 53, 22)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let mut buf = [0u8; 6];
        crate::encode_civil_time(&mut buf, at);
        assert_eq!(buf, [0x24, 0x08, 0x29, 0x10, 0x53, 0x22]);
        assert_eq!(crate::decode_civil_time(&buf).unwrap(), at);
        // 2 月 30 日回绕拒绝。
        assert!(crate::decode_civil_time(&[0x24, 0x02, 0x30, 0, 0, 0]).is_err());
        // 非法 BCD 半字节拒绝。
        assert!(crate::decode_civil_time(&[0xAF, 1, 1, 0, 0, 0]).is_err());
    }
}
