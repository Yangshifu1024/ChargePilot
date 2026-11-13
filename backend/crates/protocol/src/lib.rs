//! 设备契约层根包：跨进程契约（旧版 internal/protocol 根包的单体对应物）。
//!
//! 规则（红线）：协议码值、帧结构、校验算法一律不进本包与 domain；新增协议必须
//! 新开子 crate 实现 [`Adapter`]，不得修改根包契约。本包只定义协议无关的
//! 事件、命令与会话记录形态，字段单位在各项注释中固定。

use std::net::TcpStream;
use std::pin::Pin;
use std::sync::Arc;

/// 动态分发友好的 future 别名：契约 trait 均以 trait object 使用。
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 已解码设备事件的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// 设备状态与充电读数上报。
    Heartbeat,
    /// 设备对启动命令的回复。
    StartResult,
    /// 设备对停止命令的回复。
    StopResult,
    /// 设备主动上报充电结束。
    ChargeEnd,
    /// 设备故障上报。
    Fault,
    /// 设备对远程控制的回复（复位/升级成功时厂商不回复）。
    RemoteResult,
    /// 心跳设置、参数表与功率控制的设备应答（含拒绝原因）。
    ConfigResult,
    /// 单独上报的遥测数据（充电分档变化）。
    Telemetry,
    /// 设备校时请求。
    TimeSync,
    /// 一次在线卡刷卡上报。
    CardSwipe,
    /// 设备查询在线卡余额。
    CardBalanceQuery,
    /// 本地启动上报（投币/离线卡/按键），平台补全订单。
    LocalStart,
}

/// 单端口充电读数：时长单位秒，电量单位毫瓦时（mWh），功率单位 0.1 瓦。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortTelemetry {
    /// 端口号，从 1 开始。
    pub port: u8,
    /// 剩余授权时长（秒）。
    pub remaining_secs: u32,
    /// 已充时长（秒）。
    pub charged_seconds: u32,
    /// 剩余授权电量（mWh）。
    pub remaining_mwh: u32,
    /// 已充电量（mWh）。
    pub charged_mwh: u32,
    /// 瞬时功率（0.1 W）。
    pub power_deci_watts: u32,
}

/// 设备事件的解码结果及原始载荷，供存储和业务处理使用。
/// 时间字段为协议民用时区（UTC+8）解析后的时刻。
#[derive(Debug, Clone, Default)]
pub struct Event {
    /// 事件唯一标识（服务端生成）。
    pub event_id: String,
    /// 在线卡卡号（刷卡/余额查询事件）。
    pub card_number: u32,
    /// 设备全局唯一编号（注册板号）。
    pub device_id: String,
    /// 事件类别。
    pub kind: Option<EventType>,
    /// 端口号（从 1 开始；0xFF 表示整机）。
    pub port: u8,
    /// 设备命令结果码（启动/停止/配置应答原值）。
    pub result_code: u8,
    /// 故障码原值（故障事件）。
    pub fault_code: u8,
    /// 结束事件累计电量（Wh；入库时再除以 1000 转 kWh）。
    pub energy_wh: u32,
    /// 瞬时功率（0.1 W）。
    pub power_deci_watts: u32,
    /// 结束停止原因码（设备词表）。
    pub stop_reason: u8,
    /// 消费类型码（投币/离线卡/扫码/在线卡/免费）。
    pub consumer_type: u8,
    /// 服务端接收时刻。
    pub received_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 原始载荷（帧 DATA 段），供审计与重放。
    pub raw_payload: Vec<u8>,
    /// 帧内会话号回显。
    pub session_id: [u8; 6],
    /// 信号强度。
    pub signal: u8,
    /// 整机状态位（位 0 高温、位 1 烟雾、位 2 输入电压异常）。
    pub device_status: u8,
    /// 输入电压（V）。
    pub voltage_v: u16,
    /// 温度（摄氏）。
    pub temperature_c: i16,
    /// 各端口状态列表（下标 = 端口号-1）。
    pub port_states: Vec<u8>,
    /// 充电端口遥测列表。
    pub charging_ports: Vec<PortTelemetry>,
    /// 设备订单号（BCD 解码后；本地启动/结束事件）。
    pub order_number: String,
    /// 结束事件累计充电秒数。
    pub charged_seconds: u32,
    /// 结束事件设备侧开始时刻。
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 结束事件设备侧结束时刻。
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 结束事件上报的诊断金额（分；只作诊断，不作为入账依据）。
    pub amount_cents: i64,
    /// 在线卡拒绝/应答的余额显示值（0.1 元）。
    pub card_balance_units: u16,
    /// 命令关联的设备订单号（8 字节 BCD 原文）。
    pub order_bcd: [u8; 8],
}

/// 平台下发命令的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// 远程启动（含卡号/余额以支持在线卡）。
    Start,
    /// 远程停止。
    Stop,
    /// 主板复位。
    Reboot,
    /// 在线卡拒绝（余额不足/无效卡）。
    CardDenied,
    /// 在线卡余额应答。
    CardBalance,
}

/// 平台下发命令：由会话层编码为具体协议帧并串行写入设备。
#[derive(Debug, Clone)]
pub struct Command {
    /// 命令类别。
    pub kind: CommandKind,
    /// 端口号（从 1 开始）。
    pub port: u8,
    /// 帧会话号（服务端分配，设备应答须带回）。
    pub session_id: [u8; 6],
    /// 设备订单号（8 字节 BCD 原文，启动命令）。
    pub order_bcd: [u8; 8],
    /// 充电模式码（0 时间 / 1 电量 / 4 平台计费；由协议适配器校验）。
    pub mode: u8,
    /// 启动额度（时间模式为分钟，电量模式为 0.001 kWh）。
    pub quantity: u16,
    /// 消费类型码（2 扫码 / 3 在线卡）。
    pub consumer_type: u8,
    /// 在线卡卡号（非在线卡启动为 0）。
    pub card_number: u32,
    /// 卡余额显示值（0.1 元）。
    pub card_balance_units: u16,
    /// 卡无效标记（拒绝/余额应答）。
    pub card_invalid: bool,
}

/// 连接结束原因（device_session.close_reason 的稳定词表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    /// 设备关闭连接。
    DeviceClosed,
    /// 读取设备帧超时。
    ReadTimeout,
    /// 读取设备帧失败。
    ReadError,
    /// 设备帧或注册顺序无效。
    ProtocolError,
    /// 设备注册被拒绝。
    Rejected,
    /// 连接被同设备的新连接替换（互踢）。
    Replaced,
    /// 服务停机取消连接处理。
    ContextEnded,
    /// 写入会话记录时连接已不可达，由网关侧补记。
    Abandoned,
}

impl CloseReason {
    /// 持久化用字符串标记。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DeviceClosed => "device_closed",
            Self::ReadTimeout => "read_timeout",
            Self::ReadError => "read_error",
            Self::ProtocolError => "protocol_error",
            Self::Rejected => "rejected",
            Self::Replaced => "replaced",
            Self::ContextEnded => "context_ended",
            Self::Abandoned => "abandoned",
        }
    }
}

/// 设备注册信息（A0 帧）。
#[derive(Debug, Clone)]
pub struct Registration {
    /// 协议适配器名（如 "dc589"）。
    pub protocol: &'static str,
    /// 设备全局唯一编号。
    pub device_id: String,
    /// 硬件版本（ASCII 原文，第 3、4 字符表示端口数）。
    pub hardware_version: String,
    /// 软件版本（"软件ID:版本号"）。
    pub software_version: String,
    /// 服务端接收时刻。
    pub received_at: chrono::DateTime<chrono::Utc>,
}

/// 会话审计记录：连接结束时持久化的时间、地址与流量统计。
#[derive(Debug, Clone, Default)]
pub struct SessionRecord {
    /// 服务端生成的会话标识。
    pub session_id: String,
    /// 设备编号。
    pub device_id: String,
    /// 对端地址（host，按列宽截断）。
    pub remote_addr: String,
    /// 连接开始时刻。
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 最近一帧时刻。
    pub last_active: Option<chrono::DateTime<chrono::Utc>>,
    /// 连接结束时刻。
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 结束原因。
    pub close_reason: String,
    /// 接收字节数（载荷）。
    pub bytes_in: i64,
    /// 发送字节数（载荷）。
    pub bytes_out: i64,
    /// 接收帧数。
    pub frames_in: i64,
    /// 发送帧数。
    pub frames_out: i64,
    /// 收到但未实现的命令字节（升序去重）。
    pub unknown_commands: Vec<u8>,
}

/// 事件接收口：协议适配器把设备事件与注册交还运行时。
/// 事件持久化成功后适配器才向设备确认帧。
pub trait Sink: Send + Sync {
    /// 设备注册；返回错误表示拒绝（设备重试）。
    fn register(&self, registration: Registration) -> BoxFut<anyhow::Result<()>>;
    /// 记录一条已解码事件；返回错误表示存储失败（连接按错误收尾）。
    fn record(&self, event: Event) -> BoxFut<anyhow::Result<()>>;
}

/// 会话审计存储（可选能力）：连接结束后持久化审计记录。
pub trait SessionRecorder: Send + Sync {
    /// 保存一条会话审计记录。
    fn record_session(&self, record: SessionRecord) -> BoxFut<anyhow::Result<()>>;
}

/// 命令发送会话：运行时向单个设备连接串行下发命令。
pub trait DeviceSession: Send + Sync {
    /// 编码并写入命令；写入失败表示连接不可用。
    fn send(&self, command: Command) -> BoxFut<anyhow::Result<()>>;
    /// 关闭设备连接（互踢时由新连接调用）。
    fn close(&self) -> BoxFut<()>;
}

/// 会话路由：登记设备到会话的映射，支持同设备互踢。
pub trait SessionRouter: Send + Sync {
    /// 登记新会话；替换同设备旧会话并对其执行 on_replaced 回调。
    /// 返回是否替换了旧会话（首次登记为 false）。
    fn attach(
        &self,
        device_id: &str,
        session: Arc<dyn DeviceSession>,
        on_replaced: Arc<dyn Fn() + Send + Sync>,
    ) -> BoxFut<bool>;
    /// 按设备号取当前会话（无在线会话返回 None）。
    fn get(&self, device_id: &str) -> BoxFut<Option<Arc<dyn DeviceSession>>>;
    /// 连接结束时撤销登记（仅当仍是传入会话时移除）。
    fn detach(&self, device_id: &str, session: &Arc<dyn DeviceSession>) -> BoxFut<()>;
}

/// 设备协议适配器：每种协议一个实现（当前仅 dc589）。
pub trait Adapter: Send + Sync {
    /// 协议名（如 "dc589"），用于路由与会话登记。
    fn name(&self) -> &'static str;

    /// 接管一条设备 TCP 连接，直到连接结束。事件经 sink 交还运行时。
    fn serve_conn(
        self: Arc<Self>,
        stream: TcpStream,
        sink: Arc<dyn Sink>,
        recorder: Option<Arc<dyn SessionRecorder>>,
        router: Option<Arc<dyn SessionRouter>>,
    ) -> BoxFut<std::io::Result<()>>;
}
