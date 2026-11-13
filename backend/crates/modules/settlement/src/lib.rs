//! 业务域模块：settlement（计价、账单、发票与退款）。
//! 批 2 先行：退款记录创建与在途聚合；计费批次随批 5 落地。
//! 表归属：charge_bill、charge_billing_job、refund_record 等（docs/design/schema.md）。

pub mod recorder;
