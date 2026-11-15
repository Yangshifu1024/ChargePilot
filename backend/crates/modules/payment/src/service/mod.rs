//! payment 域业务用例与渠道适配：支付成功事务、下单冻结、充值桥、
//! 模拟确认、扫码契约与支付渠道实现。SQL 在 repo/。

pub mod apply;
pub mod channel;
pub mod checkout;
pub mod recharge_bridge;
pub mod scan;
pub mod simulation;
pub mod wechat;
#[cfg(test)]
mod wechat_test;
