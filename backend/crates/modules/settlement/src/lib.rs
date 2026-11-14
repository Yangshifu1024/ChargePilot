//! 业务域模块：settlement（计费批次、退款执行、退款记录与账单）。
//! 跨域依赖（票 21 审计）：ops,order,payment,wallet——操作员实时权限与引导账号（ops）；订单视图与事件登记（order）；支付单状态推进与退款锁（payment）；余额退回入账（wallet）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 表归属：charge_billing_job、charge_fee_receipt、charge_bill、refund_record
//! 等（docs/design/schema.md）。计费入口复用 domain 的 settle_session（批 1
//! 差分验证）；退款执行器在 simulation 通道直接成功，真实渠道随批 6 接入。
//! 锁序：计费为 订单→支付单→退款在途；执行器为 退款记录→支付单→（钱包）。

pub mod bill;
pub mod billing;
pub mod invoice_admin;
pub mod meter_review;
pub mod recorder;
pub mod refund_admin;
pub mod refund_executor;
pub mod user_invoice;

/// 审计上下文视图：由 HTTP 层传入，写事务内审计时使用（旧版 ops.AuditContext）。
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
