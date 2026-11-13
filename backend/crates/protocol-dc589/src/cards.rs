//! 在线卡帧编解码：刷卡上报（B6）、拒绝（BD）、余额查询（D0）与应答（D1）。
//! 余额显示单位 0.1 元；钱包记账仍以分为准。

use crate::commands::PayloadError;
use crate::frame::Frame;

/// B6 刷卡上报：端口、卡号与设备本地扣费配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSwipe {
    /// 端口号（从 1 开始）。
    pub port: u8,
    /// 在线卡卡号。
    pub card_number: u32,
    /// 设备本地设置的扣费额（0.1 元）；不是平台实际收取的金额。
    pub local_debit_tenths: u8,
}

/// 解码 B6 的端口、卡号和本地扣费值。
/// 四个保留字节不包含事件序号或拔卡标志；事件去重由接收方处理。
pub fn parse_card_swipe(command: u8, data: &[u8]) -> Result<CardSwipe, PayloadError> {
    if command != 0xB6 || data.len() != 10 || data[0] == 0 {
        return Err(PayloadError);
    }
    let card = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    if card == 0 {
        return Err(PayloadError);
    }
    Ok(CardSwipe {
        port: data[0],
        card_number: card,
        local_debit_tenths: data[5],
    })
}

/// 解码 D0 余额查询的卡号；卡号零值留给调用方判断。
pub fn parse_card_balance_query(command: u8, data: &[u8]) -> Result<u32, PayloadError> {
    if command != 0xD0 || data.len() != 5 || data[0] != 0 {
        return Err(PayloadError);
    }
    Ok(u32::from_le_bytes([data[1], data[2], data[3], data[4]]))
}

/// 编码 D1 余额应答；卡无效时清零余额，余额单位 0.1 元。
pub fn build_card_balance_reply(
    session: [u8; 6],
    card: u32,
    valid: bool,
    units: u16,
) -> Result<Frame, PayloadError> {
    if card == 0 && valid {
        return Err(PayloadError);
    }
    let mut d = vec![0u8; 7];
    let (flag, units) = if valid { (0, units) } else { (1, 0) };
    d[0] = flag;
    d[1..5].copy_from_slice(&card.to_le_bytes());
    d[5..7].copy_from_slice(&units.to_le_bytes());
    Ok(Frame::new(0xD1, session, d))
}

/// 编码 BD 刷卡拒绝；无效卡清零余额，余额不足时可保留卡余额。
pub fn build_card_denied(
    session: [u8; 6],
    card: u32,
    invalid: bool,
    units: u16,
) -> Result<Frame, PayloadError> {
    if card == 0 {
        return Err(PayloadError);
    }
    let mut d = vec![0u8; 9];
    let (reason, units) = if invalid { (1u8, 0u16) } else { (0u8, units) };
    d[0] = reason;
    d[1..5].copy_from_slice(&card.to_le_bytes());
    d[5..7].copy_from_slice(&units.to_le_bytes());
    Ok(Frame::new(0xBD, session, d))
}

/// 把分换算为 0.1 元显示值，向下取整并限制为 65535。
/// 此值仅用于设备显示，钱包仍以分记账。
#[must_use]
pub fn card_balance_units(cents: i64) -> u16 {
    if cents <= 0 {
        return 0;
    }
    u16::try_from(cents / 10).unwrap_or(65535)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_swipe_golden() {
        // 协议文档 B6 示例：端口 2，卡号 17564161，扣费 1 元（0.1 元 ×10）。
        let data = [0x02, 0x01, 0x02, 0x0C, 0x01, 0x0A, 0x00, 0x00, 0x00, 0x00];
        let swipe = parse_card_swipe(0xB6, &data).unwrap();
        assert_eq!(swipe.port, 2);
        assert_eq!(swipe.card_number, 17564161);
        assert_eq!(swipe.local_debit_tenths, 10);
        // 端口 0 拒绝；0xFF（端口号错误标志）按旧版语义原样透传给上层判断。
        assert_eq!(
            parse_card_swipe(0xB6, &[0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(PayloadError)
        );
    }

    #[test]
    fn balance_query_and_replies() {
        // D0 示例：查询全零卡号（调用方判断）。
        let card = parse_card_balance_query(0xD0, &[0, 0, 0, 0, 0]).unwrap();
        assert_eq!(card, 0);
        // D1 示例：有效卡 1711473153，余额 10 元（100 × 0.1 元）。
        let reply = build_card_balance_reply([0; 6], 0x66030201, true, 100).unwrap();
        assert_eq!(reply.data[0], 0);
        assert_eq!(reply.data[1..5], 0x66030201u32.to_le_bytes());
        // BD 无效卡：余额清零。
        let denied = build_card_denied([0; 6], 5, true, 100).unwrap();
        assert_eq!(denied.data[0], 1);
        assert_eq!(&denied.data[5..7], &0u16.to_le_bytes());
        // BD 卡号必填。
        assert_eq!(build_card_denied([0; 6], 0, false, 0), Err(PayloadError));
        assert_eq!(
            build_card_balance_reply([0; 6], 0, true, 0),
            Err(PayloadError)
        );
    }

    #[test]
    fn balance_units_conversion() {
        // 分 → 0.1 元：向下取整、上限 65535、非正归零。
        assert_eq!(card_balance_units(105), 10);
        assert_eq!(card_balance_units(9), 0);
        assert_eq!(card_balance_units(0), 0);
        assert_eq!(card_balance_units(-5), 0);
        assert_eq!(card_balance_units(700_000_000), 65535);
    }
}
