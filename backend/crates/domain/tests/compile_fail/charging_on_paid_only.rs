// 反例：charging 状态上调用 activate（paid → charging 迁移）必须编译失败。
// 该方法只定义在 PaidOrder 上——类型层拒绝非法迁移。
//
use chargepilot_domain::charging::state::{OrderId, StateEvidence, evidence_of};
use chargepilot_domain::order::ChargeOrderStatus;

fn main() {
    let charging = evidence_of(OrderId(1), ChargeOrderStatus::Charging)
        .and_then(StateEvidence::into_charging)
        .unwrap_or_else(|| unreachable!());
    let _ = charging.activate(); //~ERROR no method named `activate`
}
