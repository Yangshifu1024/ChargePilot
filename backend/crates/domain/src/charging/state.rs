//! 充电订单状态机：类型层拒绝非法迁移（迁移图见 `docs/design/state-machine.md`）。
//!
//! 设计：每个 DB 状态一个 newtype，迁移方法只定义在合法 from 状态的类型上——
//! 在非法 from 状态上调用迁移方法会直接编译失败（trybuild 反例 `tests/compile_fail/`）。
//! 进入 `Refunded` 一律要求持有 `RefundingOrder` 证据；`PendingPayment` / `Cancelled` /
//! `Failed` 为保留取值，不提供任何进入方法（库内无写入路径）。
//!
//! **证据构造收敛**：状态证据字段私有，唯一构造入口是 [`evidence_of`]——
//! 以数据库读到的 [`ChargeOrderStatus`] 分派对应证据类型，证据必然对应行内真实
//! 状态；仓储迁移函数消费 from 证据、返回目标证据，字面构造在编译层不可再写出。

use crate::order::ChargeOrderStatus;

/// 充电订单 ID（号段分配器分配，u64 与 DB bigint unsigned 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderId(pub u64);

/// 状态 `paid`：已支付待启动。仅由 [`evidence_of`] 按 DB 状态 `paid` 构造。
#[derive(Debug)]
pub struct PaidOrder {
    id: OrderId,
}
// 构造来源：支付回调 / 刷卡扣款事务（创建即 paid）；启动被拒路径
// （回执失败 / 超时兜底）经 `mark_start_rejected` 消费本证据类型。

/// 状态 `charging`：设备启动确认后的充电中。
#[derive(Debug)]
pub struct ChargingOrder {
    id: OrderId,
}

/// 状态 `completed`：结束回执确认（同事务入队计费任务）。
#[derive(Debug)]
pub struct CompletedOrder {
    id: OrderId,
}

/// 状态 `refunding`：退款流程在途。
#[derive(Debug)]
pub struct RefundingOrder {
    id: OrderId,
}

/// 状态 `refunded`：退款完成终态。
#[derive(Debug)]
pub struct RefundedOrder {
    id: OrderId,
}

impl PaidOrder {
    pub fn id(&self) -> OrderId {
        self.id
    }
}

impl ChargingOrder {
    pub fn id(&self) -> OrderId {
        self.id
    }
}

impl CompletedOrder {
    pub fn id(&self) -> OrderId {
        self.id
    }
}

impl RefundingOrder {
    pub fn id(&self) -> OrderId {
        self.id
    }
}

impl RefundedOrder {
    pub fn id(&self) -> OrderId {
        self.id
    }
}

/// 数据库读出的状态证据：五态证据的枚举视图（[`evidence_of`] 的返回形态）。
#[derive(Debug)]
pub enum StateEvidence {
    Paid(PaidOrder),
    Charging(ChargingOrder),
    Completed(CompletedOrder),
    Refunding(RefundingOrder),
    Refunded(RefundedOrder),
}

impl StateEvidence {
    pub fn into_paid(self) -> Option<PaidOrder> {
        match self {
            StateEvidence::Paid(v) => Some(v),
            StateEvidence::Charging(_) => None,
            StateEvidence::Completed(_) => None,
            StateEvidence::Refunding(_) => None,
            StateEvidence::Refunded(_) => None,
        }
    }

    pub fn into_charging(self) -> Option<ChargingOrder> {
        match self {
            StateEvidence::Charging(v) => Some(v),
            StateEvidence::Paid(_) => None,
            StateEvidence::Completed(_) => None,
            StateEvidence::Refunding(_) => None,
            StateEvidence::Refunded(_) => None,
        }
    }

    pub fn into_completed(self) -> Option<CompletedOrder> {
        match self {
            StateEvidence::Completed(v) => Some(v),
            StateEvidence::Paid(_) => None,
            StateEvidence::Charging(_) => None,
            StateEvidence::Refunding(_) => None,
            StateEvidence::Refunded(_) => None,
        }
    }

    pub fn into_refunding(self) -> Option<RefundingOrder> {
        match self {
            StateEvidence::Refunding(v) => Some(v),
            StateEvidence::Paid(_) => None,
            StateEvidence::Charging(_) => None,
            StateEvidence::Completed(_) => None,
            StateEvidence::Refunded(_) => None,
        }
    }

    pub fn into_refunded(self) -> Option<RefundedOrder> {
        match self {
            StateEvidence::Refunded(v) => Some(v),
            StateEvidence::Paid(_) => None,
            StateEvidence::Charging(_) => None,
            StateEvidence::Completed(_) => None,
            StateEvidence::Refunding(_) => None,
        }
    }
}

/// 状态证据的唯一构造入口：按数据库读到的状态分派。
/// 保留取值（`pending_payment` / `cancelled` / `failed`）无证据——它们没有
/// 迁移出口，调用方必须显式处理 `None`。
pub fn evidence_of(id: OrderId, status: ChargeOrderStatus) -> Option<StateEvidence> {
    match status {
        ChargeOrderStatus::Paid => Some(StateEvidence::Paid(PaidOrder { id })),
        ChargeOrderStatus::Charging => Some(StateEvidence::Charging(ChargingOrder { id })),
        ChargeOrderStatus::Completed => Some(StateEvidence::Completed(CompletedOrder { id })),
        ChargeOrderStatus::Refunding => Some(StateEvidence::Refunding(RefundingOrder { id })),
        ChargeOrderStatus::Refunded => Some(StateEvidence::Refunded(RefundedOrder { id })),
        ChargeOrderStatus::PendingPayment
        | ChargeOrderStatus::Cancelled
        | ChargeOrderStatus::Failed => None,
    }
}

/// 迁移 1：paid → charging（设备启动成功回执；DB 谓词 status='paid' + RowsAffected==1）。
impl PaidOrder {
    pub fn activate(self) -> ChargingOrder {
        ChargingOrder { id: self.id }
    }
}

/// 迁移 2：paid → refunding（设备拒绝启动回执，记 failure_reason）。
impl PaidOrder {
    pub fn mark_start_rejected(self, _failure_reason: &str) -> RefundingOrder {
        RefundingOrder { id: self.id }
    }
}

/// 迁移 3：charging → completed（设备结束回执，同事务入队计费任务）。
impl ChargingOrder {
    pub fn complete(self) -> CompletedOrder {
        CompletedOrder { id: self.id }
    }
}

/// 迁移 4：completed → refunding（渠道余款退款登记）。
impl CompletedOrder {
    pub fn request_channel_refund(self) -> RefundingOrder {
        RefundingOrder { id: self.id }
    }
}

/// 迁移 5：refunding → refunded（渠道退款全额成功回执；必须经 refunding 证据类型）。
impl RefundingOrder {
    pub fn settle_refunded(self) -> RefundedOrder {
        RefundedOrder { id: self.id }
    }
}

/// 迁移 6：refunding → completed（渠道退款部分成功且 ended_at 非空时收尾恢复）。
impl RefundingOrder {
    pub fn restore_completed(self) -> CompletedOrder {
        CompletedOrder { id: self.id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 合法迁移路径的类型层样例：证据必须经 evidence_of 分派。
    #[test]
    fn mainline_transitions_are_expressible() {
        let paid = evidence_of(OrderId(1), ChargeOrderStatus::Paid)
            .and_then(StateEvidence::into_paid)
            .expect("paid 有证据");
        let charging = paid.activate(); // paid → charging
        let completed = charging.complete(); // charging → completed
        let refunding = completed.request_channel_refund(); // completed → refunding
        let _refunded = refunding.settle_refunded(); // refunding → refunded
        // 迁移 6：渠道退款部分成功时 refunding 可恢复 completed（第二出口）。
        let refunding2 = evidence_of(OrderId(1), ChargeOrderStatus::Completed)
            .and_then(StateEvidence::into_completed)
            .expect("completed 有证据")
            .request_channel_refund();
        let _completed_again = refunding2.restore_completed(); // refunding → completed
    }

    #[test]
    fn start_rejected_path_is_expressible() {
        let paid = evidence_of(OrderId(2), ChargeOrderStatus::Paid)
            .and_then(StateEvidence::into_paid)
            .expect("paid 有证据");
        let refunding = paid.mark_start_rejected("device START rejected: 2");
        // 余额支付路径在同事务内走完 refunding → refunded。
        let _refunded = refunding.settle_refunded();
    }

    // 保留取值无证据：pending_payment / cancelled / failed 返回 None，
    // 调用方显式处理——不可字面构造绕过。
    #[test]
    fn reserved_states_have_no_evidence() {
        for status in [
            ChargeOrderStatus::PendingPayment,
            ChargeOrderStatus::Cancelled,
            ChargeOrderStatus::Failed,
        ] {
            assert!(
                evidence_of(OrderId(9), status).is_none(),
                "{status:?} 无证据"
            );
        }
    }
}