//! 设备契约层根包：跨进程契约（对应旧版 internal/protocol 根包）。
//!
//! 规则（AGENTS 红线）：协议码值、帧结构、校验算法一律不进 domain；新增协议必须
//! 新开子目录实现 `Adapter`，不得修改根包。具体 trait 面在阶段 06 定稿，
//! 当前锁定最小形态供 gateway-runtime 骨架引用。

use std::future::Future;
use std::net::TcpStream;
use std::sync::Arc;

/// 事件接收口：协议适配器把设备事件投递给运行时（落库 + 转领域事件）。
pub trait Sink: Send + Sync {}

/// 设备协议适配器：每种协议一个实现（当前仅 dc589）。
pub trait Adapter: Send + Sync {
    /// 协议名（如 "dc589"），用于路由与会话登记。
    fn name(&self) -> &'static str;

    /// 接管一条设备 TCP 连接，直到连接结束。事件经 sink 交还运行时。
    fn serve_conn(
        self: Arc<Self>,
        stream: TcpStream,
        sink: Arc<dyn Sink>,
    ) -> impl Future<Output = std::io::Result<()>> + Send;
}
