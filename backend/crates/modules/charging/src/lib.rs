//! charging 域：充电生命周期状态机的事件驱动实现（ADR 002）。
//! 跨域依赖（票 21 审计）：card,order——刷卡首购操作确认与卡会话释放（card）；订单生命周期推进与事件登记（order）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 支付成功（charge_payment_confirmed）→ 启动命令与端口预占 → 设备回执
//! starting/charging → 用户停止/自动停机（计费截止冻结）→ 结束回执
//! （订单完成 + 计费任务）→ 超时兜底扫描接管卡单。
//! 表归属：charge_command、charge_stop_command、charge_start_receipt、
//! charge_end_receipt、charge_billing_cutoff、active_port_charge
//! （charge_billing_job 由 settlement 结算批次消费，本域只登记）。
//!
//! 幂等红线：每张回执表同命令同内容重放静默确认、不同内容冲突；
//! 端口释放必须匹配订单；状态推进全部条件更新（RowsAffected==1）。

pub use error::{ReceiptError, StopError};

pub mod error;
pub mod repo;
pub mod service;
pub mod sink;

pub use service::dispatch::StartDispatcher;
pub use service::receipts::{
    EndReceiptInput, StartReceiptInput, apply_end_receipt, apply_start_receipt,
};
pub use service::stop::{ActiveStop, StopService};
pub use service::{dispatch, receipts, stop, sweeper};
pub use sink::{CommandSink, SimulationSink, StartCommand};

/// dyn 兼容 Future 别名的公共出口（模块内部各自引用）。
pub mod futures_bounds {
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}
