//! 本地参数表（C3/C4/C5/C6）与功率控制（E0/E1）编解码。
//! 刷卡时长/电量字段的单位由运行模式决定：时间模式为分钟，
//! 电量模式为 0.01 kWh；通过访问器按模式解释，避免字节混用单位。

use crate::commands::PayloadError;
use crate::frame::Frame;

/// 空闲设备心跳周期为 60 秒，充电时为 15 秒。
/// 读超时按连续缺失三次心跳计算，随申请周期调整。
pub const DEFAULT_HEARTBEAT_SECONDS: u16 = 60;
/// 充电设备请求使用的 15 秒心跳周期。
pub const CHARGING_HEARTBEAT_SECONDS: u16 = 15;
/// 设备连续未收到心跳应答后重新登录的次数阈值。
pub const MISSED_HEARTBEATS_BEFORE_RESET: u32 = 3;

/// 主板本地参数表（五档布局，33 字节载荷；八档双口变体不支持）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigTable {
    /// 运行模式：0 时间优先、1 时间卡优先、2 免费、3 电量优先、4 电量卡优先。
    pub run_mode: u8,
    /// 音量（0-8；平台不改动，固定 4）。
    pub volume: u8,
    /// 本地投币一次时间/电量（时间模式分钟，电量模式 0.01 kWh）。
    pub local_coin_time: u16,
    /// 本地刷卡一次时间/电量（单位随模式）。
    pub local_card_time: u16,
    /// 刷卡扣费金额（分；0-2500 且为 10 的倍数）。
    pub card_amount_cents: u16,
    /// 刷卡退费：0 不退、1 退。
    pub card_refund: u8,
    /// 各档功率上限（W；递增，否则主板拒绝）。
    pub tier_watts: [u16; 5],
    /// 各档折扣百分比（1-100；第一档固定 100）。
    pub tier_ratio_percent: [u8; 5],
    /// 充满自停：0 关闭、1 开启。
    pub stop_when_full: u8,
    /// 浮充功率（0.1 W；1-50 W）。
    pub float_deci_watts: u16,
    /// 浮充时间（秒；120-10800）。
    pub float_seconds: u16,
    /// 拔除检查时间（秒；5-3600）。
    pub remove_seconds: u16,
    /// 温度保护值（0xFF 关闭；50-100 摄氏）。
    pub temperature_guard: u8,
}

impl Default for ConfigTable {
    fn default() -> Self {
        // 附件一默认值。
        Self {
            run_mode: 0,
            volume: 4,
            local_coin_time: 240,
            local_card_time: 240,
            card_amount_cents: 100,
            card_refund: 0,
            tier_watts: [100, 200, 300, 400, 500],
            tier_ratio_percent: [100, 100, 100, 100, 100],
            stop_when_full: 1,
            float_deci_watts: 100,
            float_seconds: 1800,
            remove_seconds: 300,
            temperature_guard: 0xFF,
        }
    }
}

impl ConfigTable {
    /// 报告表内刷卡时间/电量字段是否按电量读取（模式 3/4）。
    fn card_time_is_energy(&self) -> bool {
        self.run_mode == 3 || self.run_mode == 4
    }

    /// 时间模式下把刷卡额度解释为分钟；电量模式返回 None。
    #[must_use]
    pub fn card_minutes(&self) -> Option<u16> {
        if self.card_time_is_energy() {
            None
        } else {
            Some(self.local_card_time)
        }
    }

    /// 电量模式下把刷卡额度解释为 mWh（0.01 kWh 计数 ×10）；时间模式 None。
    #[must_use]
    pub fn card_milli_wh(&self) -> Option<u64> {
        if self.card_time_is_energy() {
            Some(u64::from(self.local_card_time) * 10)
        } else {
            None
        }
    }

    /// 按固件允许的取值范围校验参数表，拒绝无法被设备接受的配置。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.run_mode > 4 {
            return Err(ConfigError("设备运行模式超出范围"));
        }
        if self.local_coin_time > 999 || self.local_card_time > 999 {
            return Err(ConfigError("本地时间/电量超出范围"));
        }
        if self.card_amount_cents > 2500 {
            return Err(ConfigError("刷卡扣费金额超出范围 0.0-25.0 元"));
        }
        if self.card_amount_cents % 10 != 0 {
            return Err(ConfigError("刷卡扣费金额必须为 10 分的整数倍"));
        }
        if self.card_refund > 1 || self.stop_when_full > 1 {
            return Err(ConfigError("开关值超出范围"));
        }
        if self.temperature_guard != 0xFF && !(50..=100).contains(&self.temperature_guard) {
            return Err(ConfigError("温度保护值超出范围 50-100，或 0xFF 关闭"));
        }
        if !(120..=10800).contains(&self.float_seconds) {
            return Err(ConfigError("浮充时间超出范围 120-10800 秒"));
        }
        if !(10..=500).contains(&self.float_deci_watts) {
            return Err(ConfigError("浮充功率超出范围 1-50W"));
        }
        if !(5..=3600).contains(&self.remove_seconds) {
            return Err(ConfigError("移除时间超出范围 5-3600 秒"));
        }
        for i in 1..self.tier_watts.len() {
            if self.tier_watts[i] > 0 && self.tier_watts[i] <= self.tier_watts[i - 1] {
                return Err(ConfigError("高档位功率必须大于低档位"));
            }
        }
        Ok(())
    }
}

/// 参数表校验/设备拒绝错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// C4 设备拒绝码 → 越界字段名（协议附录错误码表）。
#[must_use]
pub fn config_error_field(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("成功"),
        1 => Some("设备运行模式超出范围"),
        2 => Some("音量超出范围"),
        3 => Some("本地投币时间/电量超出范围"),
        4 => Some("本地刷卡时间/电量超出范围"),
        5 => Some("刷卡扣费金额超出范围"),
        6 => Some("刷卡退费设置错误"),
        7 => Some("分档功率设置错误"),
        8 => Some("分档比例值设置错误"),
        9 => Some("充满自停设置错误"),
        10 => Some("浮充功率超出范围"),
        11 => Some("浮充时间超出范围"),
        12 => Some("充电器移除检查时间超出范围"),
        13 => Some("温度保护值超出范围"),
        _ => None,
    }
}

/// 五档参数表编码后的 33 字节载荷；当前编解码仅接受此布局。
pub const CONFIG_ENCODED_LEN: usize = 33;

/// 把有效五档参数表编码为载荷（C3 下发与 C6 上报共用）。
pub fn encode_config(table: &ConfigTable) -> Result<Vec<u8>, ConfigError> {
    table.validate()?;
    let mut data = Vec::with_capacity(CONFIG_ENCODED_LEN);
    data.push(table.run_mode);
    data.push(4); // 音量，平台不改动
    data.extend_from_slice(&table.local_coin_time.to_le_bytes());
    data.extend_from_slice(&table.local_card_time.to_le_bytes());
    data.push((table.card_amount_cents / 10) as u8);
    data.push(table.card_refund);
    for watts in &table.tier_watts {
        // 0.1 W 单位：W ×10。
        data.extend_from_slice(&(watts * 10).to_le_bytes());
    }
    // 第一档折扣按固件要求固定为 100，不使用调用方传入值。
    data.push(100);
    for ratio in &table.tier_ratio_percent[1..] {
        data.push(*ratio);
    }
    data.push(table.stop_when_full);
    data.extend_from_slice(&table.float_deci_watts.to_le_bytes());
    data.extend_from_slice(&table.float_seconds.to_le_bytes());
    data.extend_from_slice(&table.remove_seconds.to_le_bytes());
    data.extend_from_slice(&8888u16.to_le_bytes()); // 面板密码，不改动
    data.push(table.temperature_guard);
    Ok(data)
}

/// 解码五档参数表（C3 下发与 C6 上报使用相同载荷）。
pub fn decode_config(command: u8, data: &[u8]) -> Result<ConfigTable, PayloadError> {
    // C3 下发 / C6 上报；载荷固定 33 字节。
    if (command != 0xC3 && command != 0xC6) || data.len() != CONFIG_ENCODED_LEN {
        return Err(PayloadError);
    }
    let mut table = ConfigTable {
        run_mode: data[0],
        local_coin_time: u16::from_le_bytes([data[2], data[3]]),
        local_card_time: u16::from_le_bytes([data[4], data[5]]),
        card_amount_cents: u16::from(data[6]) * 10,
        card_refund: data[7],
        tier_watts: [
            u16::from_le_bytes([data[8], data[9]]) / 10,
            u16::from_le_bytes([data[10], data[11]]) / 10,
            u16::from_le_bytes([data[12], data[13]]) / 10,
            u16::from_le_bytes([data[14], data[15]]) / 10,
            u16::from_le_bytes([data[16], data[17]]) / 10,
        ],
        tier_ratio_percent: [data[18], data[19], data[20], data[21], data[22]],
        stop_when_full: data[23],
        float_deci_watts: u16::from_le_bytes([data[24], data[25]]),
        float_seconds: u16::from_le_bytes([data[26], data[27]]),
        remove_seconds: u16::from_le_bytes([data[28], data[29]]),
        temperature_guard: data[32],
        volume: data[1],
    };
    // 第一档折扣固定 100（固件要求），解码时归一。
    table.tier_ratio_percent[0] = 100;
    Ok(table)
}

/// 解析 C4 参数应答；非零错误码表示参数被拒绝（设备保留原参数）。
pub fn parse_config_ack(command: u8, data: &[u8]) -> Result<u8, PayloadError> {
    if command != 0xC4 || data.len() != 1 {
        return Err(PayloadError);
    }
    Ok(data[0])
}

/// 编码 C4 参数应答（模拟器用）；非零表示配置被拒绝。
pub fn build_config_ack(code: u8) -> Frame {
    Frame::new(0xC4, [0; 6], vec![code])
}

/// 编码 C5 读取参数请求（控制类型固定 0）。
pub fn build_read_config(session: [u8; 6]) -> Frame {
    Frame::new(0xC5, session, vec![0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let table = ConfigTable {
            run_mode: 1,
            local_coin_time: 240,
            local_card_time: 120,
            card_amount_cents: 150,
            card_refund: 1,
            tier_watts: [100, 200, 300, 400, 500],
            tier_ratio_percent: [100, 95, 90, 85, 80],
            stop_when_full: 1,
            float_deci_watts: 100,
            float_seconds: 1800,
            remove_seconds: 300,
            temperature_guard: 0xFF,
            volume: 4,
        };
        let data = encode_config(&table).unwrap();
        assert_eq!(data.len(), CONFIG_ENCODED_LEN);
        // C3 与 C6 载荷相同。
        let decoded = decode_config(0xC6, &data).unwrap();
        assert_eq!(decoded, table);
        assert_eq!(decoded.card_amount_cents, 150);
        // 刷卡额度按时间模式解释为分钟。
        assert_eq!(decoded.card_minutes(), Some(120));
        assert!(decoded.card_milli_wh().is_none());
    }

    #[test]
    fn energy_mode_accessors() {
        let table = ConfigTable {
            run_mode: 3,
            local_card_time: 100,
            ..ConfigTable::default()
        };
        assert_eq!(table.card_minutes(), None);
        assert_eq!(table.card_milli_wh(), Some(1_000), "100 × 0.01 kWh = 1 kWh");
    }

    #[test]
    fn validate_ranges() {
        // 档位递增、金额 10 分倍数、浮充范围、温度保护。
        let bad = ConfigTable {
            tier_watts: [100, 100, 300, 400, 500],
            ..ConfigTable::default()
        };
        assert!(bad.validate().is_err());
        let bad = ConfigTable {
            card_amount_cents: 105,
            ..ConfigTable::default()
        };
        assert!(bad.validate().is_err());
        let bad = ConfigTable {
            float_deci_watts: 600,
            ..ConfigTable::default()
        };
        assert!(bad.validate().is_err());
        let bad = ConfigTable {
            temperature_guard: 49,
            ..ConfigTable::default()
        };
        assert!(bad.validate().is_err());
        let ok = ConfigTable {
            temperature_guard: 0xFF,
            ..ConfigTable::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn ack_codes_named() {
        assert_eq!(parse_config_ack(0xC4, &[0]).unwrap(), 0);
        assert_eq!(parse_config_ack(0xC4, &[7]).unwrap(), 7);
        assert_eq!(config_error_field(7), Some("分档功率设置错误"));
        assert_eq!(config_error_field(99), None);
        assert_eq!(parse_config_ack(0xC4, &[0, 0]), Err(PayloadError));
    }
}
