//! 业务域模块：card（在线卡绑定、刷卡扣款、续购与操作幂等）。
//! 跨域依赖：ops,order,payment,pricing,wallet——操作员权限（ops）；订单锁与卡会话归属（order）；卡支付写入与退款渠道（payment）；计费规则快照（pricing）；余额扣款与退回（wallet）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 表归属：online_card、card_operation、card_charge、online_card_audit、
//! card_event_delivery（docs/design/schema.md）。
//!
//! 刷卡准入是设备刷卡事件的同步应答：幂等重放、时效校验、设备条件与余额核对通过后，
//! 同事务完成扣款、订单/意图/快照写入与出站事件；拒绝统一 409/2009 语义（网关映射协议拒绝码）。
//! 锁序：卡 → 钱包 → 会话 → 订单。
//!
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；既有顶级模块路径
//! （admin/swipe/user_card）经 re-export 保持，SQL 只出现在 repo/。

pub use error::{CardAdminError, SwipeError, UserCardError};

pub mod error;
pub mod repo;
pub mod service;

pub use service::{admin, swipe, user_card};

pub use repo::balance::balance_facts;
pub use repo::swipe::{
    card_charge_port_release_tx, confirm_start_operation_tx, fail_start_operation_tx,
};
pub use repo::{
    CardChargeRow, CardOperationRow, active_session_count, find_card_charge, find_online_card,
};
pub use service::swipe::{CardOperationResult, SwipeInput, SwipeService};
