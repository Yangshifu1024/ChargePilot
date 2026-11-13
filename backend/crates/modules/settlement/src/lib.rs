//! 业务域模块：settlement（计费批次、退款执行、退款记录与账单）。
//! 表归属：charge_billing_job、charge_fee_receipt、charge_bill、refund_record
//! 等（docs/design/schema.md）。计费入口复用 domain 的 settle_session（批 1
//! 差分验证）；退款执行器在 simulation 通道直接成功，真实渠道随批 6 接入。
//! 锁序：计费为 订单→支付单→退款在途；执行器为 退款记录→支付单→（钱包）。

pub mod billing;
pub mod recorder;
pub mod refund_executor;
