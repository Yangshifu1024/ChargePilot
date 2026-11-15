//! 消费类型码词表（设备端 consumer_type 字段；DC589 5.8.9 协议语义）。
//!
//! 码值封闭在本 crate：gateway-runtime 与业务模块只引用这里的具名常量，
//! 不出现裸数字。

/// 投币。
pub const COIN: u8 = 0;
/// 离线卡。
pub const OFFLINE_CARD: u8 = 1;
/// 扫码。
pub const SCAN: u8 = 2;
/// 在线卡。
pub const ONLINE_CARD: u8 = 3;
/// 免费。
pub const FREE: u8 = 4;
