//! 业务域模块：marketing（优惠券与活动）。
//! 批 2/3 交付券域主链：模板管理（[`coupon`]）、用户持券视图（[`query`]）与
//! 活动规则及事件触发的幂等发放（[`activity`]）；计价接缝与结算核销随批 4 落地。

pub mod activity;
pub mod coupon;
pub mod query;
