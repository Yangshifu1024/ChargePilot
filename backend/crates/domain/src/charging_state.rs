//! 充电订单状态机：类型层非法迁移拒绝（阶段 00 初稿，迁移图定案见 docs/design/state-machine.md）。
//!
//! 设计：每个 DB 状态一个 newtype，迁移方法只定义在合法 from 状态的类型上——
//! 在非法 from 状态上调用迁移方法会直接编译失败（trybuild 反例 `tests/compile_fail/`）。
//! 新版收紧旧版两处无保护写点：进入 `Refunded` 一律要求 from 证据类型；
//! `PendingPayment` / `Cancelled` / `Failed` 为保留取值，不提供任何进入方法（旧版零写入）。

/// 充电订单 ID（新版由号段分配器分配，u64 与 DB bigint unsigned 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderId(pub u64);

/// 状态 `paid`：已支付待启动。仅由支付回调 / 刷卡扣款事务（创建即 paid）构造。
#[derive(Debug)]
pub struct PaidOrder {
    pub id: OrderId,
}
// 构造来源：支付回调 / 刷卡扣款事务（创建即 paid）；启动被拒路径
// （回执失败 / 超时兜底）经 `mark_start_rejected` 消费本证据类型。

/// 状态 `charging`：设备启动确认后的充电中。
#[derive(Debug)]
pub struct ChargingOrder {
    pub id: OrderId,
}

/// 状态 `completed`：结束回执确认（同事务入队计费任务）。
#[derive(Debug)]
pub struct CompletedOrder {
    pub id: OrderId,
}

/// 状态 `refunding`：退款流程在途。
#[derive(Debug)]
pub struct RefundingOrder {
    pub id: OrderId,
}

/// 状态 `refunded`：退款完成终态。
#[derive(Debug)]
pub struct RefundedOrder {
    pub id: OrderId,
}

/// 迁移迁移 1：paid → charging（设备启动成功回执；DB 谓词 status='paid' + RowsAffected==1）。
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

/// 迁移 5：refunding → refunded（渠道退款全额成功回执；新版要求 from=refunding 谓词，收紧旧版无谓词写点）。
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

    // 合法迁移路径的类型层样例（行为实现在阶段 04，此处锁定迁移拓扑）
    #[test]
    fn mainline_transitions_are_expressible() {
        let paid = PaidOrder { id: OrderId(1) };
        let charging = paid.activate(); // paid → charging
        let completed = charging.complete(); // charging → completed
        let refunding = completed.request_channel_refund(); // completed → refunding
        let _refunded = refunding.settle_refunded(); // refunding → refunded
    }

    #[test]
    fn start_rejected_path_is_expressible() {
        let refunding =
            PaidOrder { id: OrderId(2) }.mark_start_rejected("device START rejected: 2");
        // 余额支付路径在同事务内继续走完 refunding → refunded（旧版迁移 3）
        let _refunded = refunding.settle_refunded();
    }
}
