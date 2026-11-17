//! 业务域模块：settlement（计费批次、退款执行、退款记录与账单）。
//! 跨域依赖：ops,order,payment,wallet——操作员实时权限与引导账号（ops）；订单视图与事件登记（order）；支付单状态推进与退款锁（payment）；余额退回入账（wallet）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 表归属：charge_billing_job、charge_fee_receipt、charge_bill、refund_record
//! 等（docs/design/schema.md）。计费入口复用 domain 的 settle_session；
//! 退款执行器在 simulation 通道直接成功，真实渠道待微信外呼接入。
//! 锁序：计费为 订单→支付单→退款在途；执行器为 退款记录→支付单→（钱包）。
//!
//! 组织（蓝图第五章）：service/ 业务用例，repo/ 表访问；既有顶级模块路径
//! （bill/billing/recorder 等）经 re-export 保持。

pub use error::{InvoiceAdminError, RefundAdminError, ReviewError, UserInvoiceError};

pub mod error;
pub mod repo;
pub mod service;

pub use repo::recorder;
pub use service::{
    bill, billing, invoice_admin, meter_review, refund_admin, refund_executor, user_invoice,
};

/// 审计上下文视图：由 HTTP 层传入，写事务内审计时使用。
/// 模块内函数以 `Option<&AuditView>` 接收；None 表示调用方不写审计（模块级测试）。
#[derive(Debug, Clone, Copy)]
pub struct AuditView<'a> {
    /// 已实时授权的操作人资料（审计记录 actor_id / actor_name）。
    pub actor: &'a chargepilot_mod_ops::Profile,
    /// 客户端 IP。
    pub client_ip: &'a str,
    /// 请求编号；业务自带幂等号时由调用处覆盖。
    pub request_id: &'a str,
}
