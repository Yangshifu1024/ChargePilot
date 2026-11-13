// 反例：charging 状态上调用 activate()（paid → charging 迁移）必须编译失败。
// 该方法只定义在 PaidOrder 上——类型层拒绝非法迁移。
use chargepilot_domain::charging_state::{ChargingOrder, OrderId};

fn main() {
    let charging = ChargingOrder { id: OrderId(1) };
    let _ = charging.activate(); //~ERROR no method named `activate`
}
