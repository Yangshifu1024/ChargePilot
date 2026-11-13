//! 设备接入运行时：TCP listener + DC589 会话 + 遥测批量管道 + 停机重试
//! （容量预算 G1–G4，阶段 06 落地）。
//!
//! 分层：会话路由（互踢）→ 协议适配器（dc589）→ 事件 sink（落库/领域事件）
//! → 命令队列（有界 mpsc + 失败退避）。业务码值零协议字面量。

pub mod command_sink;
pub mod device_events;
pub mod listener;
pub mod registry;
pub mod sink;
pub mod snapshot;
pub mod telemetry;

pub use command_sink::GatewayCommandSink;
pub use device_events::DeviceEventDispatcher;
pub use listener::DeviceListener;
pub use registry::SessionRegistry;
pub use sink::GatewaySink;
pub use snapshot::{DeviceSnapshot, SnapshotStore};
pub use telemetry::TelemetryBuffer;
