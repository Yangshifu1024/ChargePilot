//! 业务域模块：card（在线卡绑定、刷卡扣款、续购与操作幂等）。
//! 表归属：online_card、card_operation、card_charge、online_card_audit、
//! card_event_delivery（docs/design/schema.md）。
//!
//! 语义基准：旧仓库 internal/card/*（swipe/card_repo/extension）。刷卡准入是
//! 设备刷卡事件的同步应答：幂等重放、时效校验、设备条件与余额核对通过后，
//! 同事务完成扣款、订单/意图/快照写入与出站事件；拒绝统一 409/2009 语义
//! （阶段 06 网关适配器映射协议拒绝码）。
//! 锁序：卡 → 钱包 → 会话 → 订单（旧版一致；与退款预占的 钱包→支付 异径）。

pub mod repo;
pub mod swipe;

pub use repo::{active_session_count, find_card_charge, CardChargeRow, CardOperationRow};
pub use swipe::{
    card_charge_port_release_tx, confirm_start_operation_tx, fail_start_operation_tx,
    CardOperationResult, SwipeError, SwipeInput, SwipeService,
};
