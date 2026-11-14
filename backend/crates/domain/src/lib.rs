//! 纯业务规则与跨用例契约：零 IO、零框架依赖。
//!
//! 模块清单随阶段推进增补；当前承载充电订单状态机的类型层定义（阶段 00 评审基准，
//! 完整迁移图见 `docs/design/state-machine.md`）。

pub mod card;
pub mod charging_actual;
pub mod charging_cost;
pub mod charging_measured;
pub mod charging_order;
pub mod charging_scheme;
pub mod charging_settle;
pub mod charging_spec;
pub mod charging_state;
pub mod limits;
pub mod marketing;
pub mod money;
pub mod operations;
pub mod order;
pub mod page;
pub mod payment_intent;
pub mod settlement;
mod wordlist;
