//! order 域：充电订单写模型与读组合（表归属：charge_order / charge_order_pricing /
//! charge_event_log 归 order；编号规则纯函数在 domain::payment_intent）。
//! 支付成功事务由 payment 模块编排，本模块提供订单侧存储原语与读模型。
//!
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问与读模型；
//! 下面的 re-export 保持既有顶级路径不变（`month_start_of`、`feedback` 等）。
//!
//! 写侧职责注记（架构评审候选 4 定案）：订单生命周期推进的证据链（读状态
//! → 构证据 → 条件 UPDATE）封装在 repo 原语内部（`mark_charging_tx` /
//! `advance_refunded_tx` / `mark_start_rejected_tx` 范式），跨域编排者
//! （payment 创建、charging 回执、settlement 退款）只消费原语；本域
//! service/ 只承载 feedback 用例——写侧不设编排层是有意形态。

pub mod error;
pub mod repo;
pub mod service;

pub use error::{FeedbackError, FeedbackSubmitError, OrderError};
pub use repo::lifecycle::{
    LifecycleView, PaidOrderDraft, advance_refunded_tx, append_event_in_tx, complete_tx,
    create_paid_in_tx, freeze_pricing_in_tx, lock_for_lifecycle_tx, mark_charging_tx,
    mark_start_rejected_tx, month_start_of, new_charge_order_no_tx, new_payment_order_no,
    occupy_port_tx, release_port_tx,
};
pub use repo::{admin_query, user_query};
pub use service::feedback;
