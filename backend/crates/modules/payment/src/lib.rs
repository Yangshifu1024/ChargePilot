//! 业务域模块：payment（支付意图、渠道、回调与支付记录）。
//! 批 2 先行：钱包退款所需的支付来源锁定；意图/回调/渠道随批 3、微信封装随批 6。
//! 表归属：payment_order、charge_payment_intent 等（docs/design/schema.md）。

pub mod sources;
