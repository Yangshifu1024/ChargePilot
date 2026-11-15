// 反例（票 14）：状态证据的字面构造必须编译失败——字段私有，唯一构造入口
// 是 evidence_of 分派（证据必然对应 DB 真实状态），丢弃式证明不可再写出。
use chargepilot_domain::charging::state::{ChargingOrder, OrderId};

fn main() {
    let _ = ChargingOrder { id: OrderId(1) }; //~ERROR field `id` of struct `ChargingOrder` is private
}
