//! 设备命令下沉通道（旧版 charging→gateway 的窄接口对应物）。
//! 阶段 06 前由 simulation 实现闭环；接入网关后 cmd 装配替换为 TCP 下发。

/// 启动命令下沉（旧版 Gateway 接口的启动/停止受理部分）。
/// 实现必须可重入：同一 command_id 重复下发等价。
pub trait CommandSink: Send + Sync {
    /// 下发启动命令。返回 Ok 表示设备侧已受理（ack）；Err 为下发失败（可重试）。
    fn send_start<'a>(
        &'a self,
        command: &StartCommand,
    ) -> futures_bounds::BoxFuture<'a, Result<(), String>>;
    /// 下发停止命令。
    fn send_stop<'a>(
        &'a self,
        device_id: &'a str,
        port_no: u8,
        stop_command_id: &'a str,
    ) -> futures_bounds::BoxFuture<'a, Result<(), String>>;
}

/// dyn 兼容的 Future 别名（与 events/wallet 同一模式）。
pub mod futures_bounds {
    pub type BoxFuture<'a, T> =
        std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

/// 启动命令内容（charge_command 行的发送投影）。
#[derive(Debug, Clone)]
pub struct StartCommand {
    pub command_id: String,
    pub charge_order_id: u64,
    pub device_id: String,
    pub port_no: u8,
    /// 设备协议充电模式码（0 时长 / 1 电量 / 4 金额）。
    pub charge_mode: u8,
    /// 下发数量（分钟或 Wh，单位随模式）。
    pub quantity: u16,
}

/// 模拟通道：直接受理成功（阶段 07 模拟器接入后由设备模拟真实应答）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SimulationSink;

impl CommandSink for SimulationSink {
    fn send_start<'a>(
        &'a self,
        _command: &StartCommand,
    ) -> futures_bounds::BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn send_stop<'a>(
        &'a self,
        _device_id: &'a str,
        _port_no: u8,
        _stop_command_id: &'a str,
    ) -> futures_bounds::BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}
