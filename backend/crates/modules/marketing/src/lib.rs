//! 业务域模块：marketing（优惠券与活动）。
//! 跨域依赖（票 21 审计）：identity,ops——持券用户身份事实（identity）；操作员审计事务（ops）。
//! 依赖图无环（ops 为唯一根；order 不依赖 payment/wallet，无反向边）。
//! 批 2/3 交付券域主链：模板管理（[`coupon`]）、用户持券视图（[`query`]）与
//! 活动规则及事件触发的幂等发放（[`activity`]）；计价接缝与结算核销随批 4 落地。

pub mod activity;
pub mod coupon;
pub mod query;
