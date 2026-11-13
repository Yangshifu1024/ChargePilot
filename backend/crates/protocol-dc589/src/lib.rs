//! DC589 5.8.9 协议实现（帧编解码、36 条平台指令、故障码语义）。
//!
//! 红线：协议码值（0xEE、0xBB 等）、帧结构、校验算法全部封闭在本 crate，
//! gateway-runtime 与业务模块只见 [`chargepilot_protocol`] 的中性契约与
//! [`fault`] 输出的指标名。字段单位换算（Wh↔mWh、分↔0.1 元、温度偏移）
//! 也在本 crate 完成。
//!
//! 语义依据：旧仓库 `docs/protocols/dc589.md`（5.8.9）与旧版实现双重对照；
//! 原文示例帧的 LEN/SUM 勘误按该文档「原始示例帧的机械校验」节修正。

pub mod cards;
pub mod commands;
pub mod config;
pub mod device;
pub mod fault;
pub mod frame;
pub mod measurements;

use chrono::FixedOffset;

/// 协议民用时区（UTC+8）：帧中日期时间不含时区，按中国大陆解析，
/// 不依赖宿主机 TZ。
#[must_use]
pub fn china_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+8 固定偏移合法")
}

/// BCD 解码错误：出现非十进制半字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BcdError;

/// 十六进制半字节值（非法字符返回 None）。
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// BCD 解码为十进制字符串；半字节超出 0-9 报错。
pub(crate) fn decode_bcd(data: &[u8]) -> Result<String, BcdError> {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        let hi = b >> 4;
        let lo = b & 0x0f;
        if hi > 9 || lo > 9 {
            return Err(BcdError);
        }
        out.push((b'0' + hi) as char);
        out.push((b'0' + lo) as char);
    }
    Ok(out)
}

/// 十进制数字串 BCD 编码；奇数长度或非数字报错。
pub(crate) fn encode_bcd(digits: &str) -> Result<Vec<u8>, BcdError> {
    let bytes = digits.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(BcdError);
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let (hi, lo) = (pair[0], pair[1]);
        if !hi.is_ascii_digit() || !lo.is_ascii_digit() {
            return Err(BcdError);
        }
        out.push((hi - b'0') << 4 | (lo - b'0'));
    }
    Ok(out)
}

/// 模块/SIM 标识解码：BCD 优先，含非十进制半字节时按大写十六进制兼容
/// （厂商样例的 SIM 含 C 半字节，与 BCD 声明存在原文差异）。
pub(crate) fn decode_identifier(data: &[u8]) -> String {
    if let Ok(s) = decode_bcd(data) {
        return s;
    }
    data.iter().map(|b| format!("{b:02X}")).collect()
}

/// 模块/SIM 标识编码到固定宽度：十进制按 BCD，其余合法十六进制字符按
/// 半字节编码；不补零或截断。
pub(crate) fn encode_identifier(value: &str, width: usize) -> Result<Vec<u8>, BcdError> {
    if value.len() != width * 2 {
        return Err(BcdError);
    }
    if let Ok(packed) = encode_bcd(value) {
        return Ok(packed);
    }
    let bytes = value.as_bytes();
    let mut raw = Vec::with_capacity(width);
    for i in 0..width {
        let hi = hex_nibble(bytes[i * 2]).ok_or(BcdError)?;
        let lo = hex_nibble(bytes[i * 2 + 1]).ok_or(BcdError)?;
        raw.push(hi << 4 | lo);
    }
    Ok(raw)
}

/// 六字节 BCD 民用时间编码（年两位 月 日 时 分 秒）。
pub(crate) fn encode_civil_time(dst: &mut [u8], at: chrono::DateTime<chrono::Utc>) {
    use chrono::Datelike as _;
    use chrono::Timelike as _;
    let local = at.with_timezone(&china_offset());
    let values = [
        local.year() % 100,
        local.month() as i32,
        local.day() as i32,
        local.hour() as i32,
        local.minute() as i32,
        local.second() as i32,
    ];
    for (i, n) in values.iter().enumerate() {
        dst[i] = ((n / 10) << 4 | (n % 10)) as u8;
    }
}

/// 六字节 BCD 民用时间解码；非法日期报错。
pub(crate) fn decode_civil_time(data: &[u8]) -> Result<chrono::DateTime<chrono::Utc>, BcdError> {
    use chrono::{Datelike as _, TimeZone, Timelike as _};
    if data.len() != 6 {
        return Err(BcdError);
    }
    let mut values = [0i32; 6];
    for (i, b) in data.iter().enumerate() {
        let (hi, lo) = (b >> 4, b & 0x0f);
        if hi > 9 || lo > 9 {
            return Err(BcdError);
        }
        values[i] = i32::from(hi) * 10 + i32::from(lo);
    }
    let offset = china_offset();
    let naive =
        chrono::NaiveDate::from_ymd_opt(2000 + values[0], values[1] as u32, values[2] as u32)
            .ok_or(BcdError)?
            .and_hms_opt(values[3] as u32, values[4] as u32, values[5] as u32)
            .ok_or(BcdError)?;
    // 用带偏移的解析并回读校验（拒绝 2 月 30 日等回绕值）。
    let with_offset = offset
        .from_local_datetime(&naive)
        .single()
        .ok_or(BcdError)?;
    if with_offset.month() != values[1] as u32
        || with_offset.day() != values[2] as u32
        || with_offset.hour() != values[3] as u32
        || with_offset.minute() != values[4] as u32
        || with_offset.second() != values[5] as u32
    {
        return Err(BcdError);
    }
    Ok(with_offset.with_timezone(&chrono::Utc))
}
