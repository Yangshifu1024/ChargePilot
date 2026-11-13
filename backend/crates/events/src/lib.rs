//! 领域事件层：事务性写入 + 进程内投递 + 失败退避（ADR 002）。
//!
//! 骨架阶段锁定契约：事件源、类别词表、投递器 trait、表访问函数签名；
//! 投递器运行时与兜底扫描在阶段 01 实现。

use serde::{Deserialize, Serialize};

/// 事件产生域（`domain_event.source` 列词表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    User,
    Admin,
    Gateway,
}

/// 事件类别词表（`domain_event.kind`）。首批沿用旧版流名语义，后续随各域落地增补。
pub mod kind {
    pub const CHARGE_ENDED: &str = "charge_ended";
    pub const REFUND_REQUIRED: &str = "refund_required";
    pub const REFUND_SUCCEEDED: &str = "refund_succeeded";
    pub const CHARGE_SETTLED: &str = "charge_settled";
    pub const CHARGE_PAYMENT_CONFIRMED: &str = "charge_payment_confirmed";
    pub const WALLET_RECHARGE_SETTLED: &str = "wallet_recharge_settled";
    pub const DEVICE_EVENT: &str = "device_event";
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
pub trait EventDispatcher: Send + Sync {
    /// 投递单个事件。返回 Err 视为投递失败，由运行时按 `1 << min(retry, 8)` 秒退避重排。
    fn dispatch(
        &self,
        event: &EnqueuedEvent,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send;
}

/// 事务内入队：在既有业务事务上写 `domain_event` 行（status=pending）。
/// 与业务写入同事务提交是可靠投递的根基（ADR 002）；阶段 01 实现函数体。
pub mod enqueue {
    use sqlx::MySqlConnection;

    use super::{EventSource, kind};

    /// 阶段 01 实现事务内写入。当前仅锁定签名供各域评审引用。
    pub fn in_tx(
        _conn: &mut MySqlConnection,
        _source: EventSource,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> Result<(), anyhow::Error> {
        anyhow::bail!("阶段 01 实现：domain_event 事务内写入")
    }

    /// 编译期锁定首批事件类别常量与来源枚举可见性。
    #[allow(dead_code)]
    fn smoke() {
        let _ = (kind::CHARGE_ENDED, EventSource::Gateway);
    }
}
