//! 设备命令通道（G3）：charging 域的 CommandSink 真实实现——经会话注册表
//! 把启动/停止命令下发到在线设备连接；设备离线即失败（由 dispatcher
//! 标记 rejected，sweeper 兜底退款）。

use std::sync::Arc;

use crate::SessionRegistry;
use chargepilot_mod_charging::sink::CommandSink;
use chargepilot_protocol::SessionRouter as _;
use chargepilot_protocol::{Command, CommandKind};

/// 数值订单 ID 转八字节 BCD 设备订单号（左补零；超过十六位拒绝）。
pub fn numeric_order_bcd(order_id: u64) -> Result<[u8; 8], String> {
    let digits = order_id.to_string();
    if digits.len() > 16 {
        return Err("订单号超出 16 位".into());
    }
    let padded = format!("{digits:0>16}");
    let bytes = padded.as_bytes();
    let mut out = [0u8; 8];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = (pair[0] - b'0') << 4 | (pair[1] - b'0');
    }
    Ok(out)
}

/// 经会话注册表下发设备命令。
pub struct GatewayCommandSink {
    /// 会话注册表（互踢路由）。
    pub registry: Arc<SessionRegistry>,
    /// 帧会话号由服务端生成；命令下发直接复用设备连接的服务端会话。
    /// DeviceSession 的实现（DcSession）在 send 内部使用自身会话号，
    /// 此处占位全零仅用于外部关联。
    pub server_session: [u8; 6],
}

impl GatewayCommandSink {
    /// 组装。
    #[must_use]
    pub fn new(registry: Arc<SessionRegistry>) -> Self {
        Self {
            registry,
            server_session: [0; 6],
        }
    }
}

impl CommandSink for GatewayCommandSink {
    fn send_start<'a>(
        &'a self,
        command: &chargepilot_mod_charging::sink::StartCommand,
    ) -> chargepilot_mod_charging::sink::futures_bounds::BoxFuture<'a, Result<(), String>> {
        // 参数数据复制进 future：签名只约束 self 活到 'a（对齐 trait 生命周期）。
        let registry = self.registry.clone();
        let server_session = self.server_session;
        let device_id = command.device_id.clone();
        let port_no = command.port_no;
        let order_id = command.charge_order_id;
        let mode = command.charge_mode;
        let quantity = command.quantity;
        Box::pin(async move {
            let session = registry
                .get(&device_id)
                .await
                .ok_or_else(|| format!("设备离线: {device_id}"))?;
            let order_bcd = numeric_order_bcd(order_id)?;
            session
                .send(Command {
                    kind: CommandKind::Start,
                    port: port_no,
                    session_id: server_session,
                    order_bcd,
                    mode,
                    quantity,
                    consumer_type: 2, // 扫码
                    card_number: 0,
                    card_balance_units: 0,
                    card_invalid: false,
                })
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send_stop<'a>(
        &'a self,
        device_id: &str,
        port_no: u8,
        _stop_command_id: &str,
    ) -> chargepilot_mod_charging::sink::futures_bounds::BoxFuture<'a, Result<(), String>> {
        let registry = self.registry.clone();
        let server_session = self.server_session;
        let device_id = device_id.to_string();
        Box::pin(async move {
            let session = registry
                .get(&device_id)
                .await
                .ok_or_else(|| format!("设备离线: {device_id}"))?;
            session
                .send(Command {
                    kind: CommandKind::Stop,
                    port: port_no,
                    session_id: server_session,
                    order_bcd: [0; 8],
                    mode: 0,
                    quantity: 0,
                    consumer_type: 0,
                    card_number: 0,
                    card_balance_units: 0,
                    card_invalid: false,
                })
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_bcd_pads_left() {
        // 订单 ID 42 → 16 位补零 → BCD。
        let bcd = numeric_order_bcd(42).unwrap();
        assert_eq!(bcd, [0, 0, 0, 0, 0, 0, 0x00, 0x42]);
        let big = numeric_order_bcd(1234567890123456).unwrap();
        assert_eq!(big[7], 0x56);
        // 超 16 位拒绝。
        assert!(numeric_order_bcd(99_999_999_999_999_999).is_err());
    }
}
