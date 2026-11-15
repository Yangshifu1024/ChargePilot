//! 领域事件层：事务性写入 + 进程内投递 + 失败退避（ADR 002）。
//!
//! 契约见 docs/design/events.md；事件源、类别词表、投递器 trait 在此定义，
//! 表访问在 repo，投递循环在 runtime，事务内入队在 enqueue。

use serde::{Deserialize, Serialize};

/// 事件产生域（`domain_event.source` 列词表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    User,
    Admin,
    Gateway,
}

/// 事件类别词表（`domain_event.kind`，进程内约束——该列为无约束 VARCHAR，
/// 枚举只约束注册表路由与入队校验；载荷透传，未知类别保持「无处理器」路径）。
/// 名单与全仓实际写入点一一对应（架构评审候选 3：补 4 个实写值、删 3 个
/// 从无写入者的愿望值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// 充电结束回执（charging/receipts，BB 帧）。
    ChargeEnded,
    /// 计费批次结算完成（settlement/billing）。
    ChargeSettled,
    /// 支付回执确认（payment/apply、card/swipe）。
    ChargePaymentConfirmed,
    /// 支付迟到（payment/apply：订单已终态后的迟到回调）。
    ChargePaymentLate,
    /// 启动失败（charging/receipts：设备拒绝启动）。
    ChargeStartFailed,
    /// 启动超时（charging/sweeper：兜底扫描接管滞留启动）。
    ChargeStartTimeout,
    /// 退款完成（settlement/refund_executor）。
    RefundCompleted,
    /// 设备事件中转（gateway-runtime）。
    DeviceEvent,
}

impl EventKind {
    /// DB 字面量锚点（与写入侧 `kind` 字面量逐字节一致）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChargeEnded => "charge_ended",
            Self::ChargeSettled => "charge_settled",
            Self::ChargePaymentConfirmed => "charge_payment_confirmed",
            Self::ChargePaymentLate => "charge_payment_late",
            Self::ChargeStartFailed => "charge_start_failed",
            Self::ChargeStartTimeout => "charge_start_timeout",
            Self::RefundCompleted => "refund_completed",
            Self::DeviceEvent => "device_event",
        }
    }

    /// 字面量 → 类别；未知值 None（路由层按无处理器处理）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "charge_ended" => Self::ChargeEnded,
            "charge_settled" => Self::ChargeSettled,
            "charge_payment_confirmed" => Self::ChargePaymentConfirmed,
            "charge_payment_late" => Self::ChargePaymentLate,
            "charge_start_failed" => Self::ChargeStartFailed,
            "charge_start_timeout" => Self::ChargeStartTimeout,
            "refund_completed" => Self::RefundCompleted,
            "device_event" => Self::DeviceEvent,
            _ => return None,
        })
    }
}

/// 事件类别常量（旧 `kind::` 模块的兼容入口；新代码用 [`EventKind`]）。
pub mod kind {
    use super::EventKind;
    pub const CHARGE_ENDED: &str = EventKind::ChargeEnded.as_str();
    pub const CHARGE_SETTLED: &str = EventKind::ChargeSettled.as_str();
    pub const CHARGE_PAYMENT_CONFIRMED: &str = EventKind::ChargePaymentConfirmed.as_str();
    pub const CHARGE_PAYMENT_LATE: &str = EventKind::ChargePaymentLate.as_str();
    pub const CHARGE_START_FAILED: &str = EventKind::ChargeStartFailed.as_str();
    pub const CHARGE_START_TIMEOUT: &str = EventKind::ChargeStartTimeout.as_str();
    pub const REFUND_COMPLETED: &str = EventKind::RefundCompleted.as_str();
    pub const DEVICE_EVENT: &str = EventKind::DeviceEvent.as_str();
}

/// domain_event.status 列的投递状态（四值；pending 领取中、
/// published 处理器已确认、failed 待退避重排、dead 重试上限后死信终态）。
/// sqlx 三件套手写（与 domain wordlist_enum! 同款打样范式；events 是
/// domain 外的独立 crate，宏保持 pub(crate) 不跨 crate 导出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDeliveryStatus {
    Pending,
    Published,
    Failed,
    Dead,
}

impl EventDeliveryStatus {
    /// DB 字面量锚点（列无约束 VARCHAR，dead 为词表扩展值）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Published => "published",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}

impl sqlx::Type<sqlx::MySql> for EventDeliveryStatus {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <str as sqlx::Type<sqlx::MySql>>::type_info()
    }

    fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
        <str as sqlx::Type<sqlx::MySql>>::compatible(ty)
    }
}

impl<'de> sqlx::Decode<'de, sqlx::MySql> for EventDeliveryStatus {
    fn decode(value: sqlx::mysql::MySqlValueRef<'de>) -> Result<Self, sqlx::error::BoxDynError> {
        let literal = <&str as sqlx::Decode<sqlx::MySql>>::decode(value)?;
        Ok(match literal {
            "pending" => Self::Pending,
            "published" => Self::Published,
            "failed" => Self::Failed,
            "dead" => Self::Dead,
            other => {
                return Err(format!("未知字面量 `{other}`（词表 EventDeliveryStatus）").into());
            }
        })
    }
}

impl sqlx::Encode<'_, sqlx::MySql> for EventDeliveryStatus {
    fn encode_by_ref(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <&str as sqlx::Encode<sqlx::MySql>>::encode(self.as_str(), buf)
    }
}

/// 已入队待投递的事件行（`domain_event` 读模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueuedEvent {
    pub id: u64,
    pub event_id: String,
    pub source: EventSource,
    pub kind: String,
    pub payload: serde_json::Value,
    pub retry_count: u32,
}

/// 事件投递器：把入队事件送达对应处理器。
/// 实现要求：处理器幂等（ADR 002 红线——事件 at-least-once，重放不得重复扣款）。
/// 返回装箱 Future（dyn 兼容，登记表按 kind 分发）。
pub trait EventDispatcher: Send + Sync {
    /// 投递单个事件。返回 Err 视为投递失败，由运行时按 `1 << min(retry, 8)` 秒退避重排。
    fn dispatch(
        &self,
        event: &EnqueuedEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send + '_>>;
}

pub mod enqueue;
pub mod repo;
pub mod runtime;
