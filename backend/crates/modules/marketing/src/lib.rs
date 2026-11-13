//! 业务域模块：marketing（优惠券与活动）。
//! 批 2 交付券域主链：模板管理（[`coupon`]）与用户持券视图（[`query`]）；
//! 活动规则与幂等发放、计价接缝与结算核销随批 3/批 4 落地。

pub mod coupon;
pub mod query;
