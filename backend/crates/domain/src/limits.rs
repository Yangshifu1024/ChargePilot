//! 业务界限值单一真相源。
//!
//! 充电、计费、分页等各域校验共用的数值上界与下界；分页对 C 端与后台
//! 有意采用双轨上界（[`USER_PAGE_MAX`] 10 万 / [`ADMIN_PAGE_MAX`] 100 万）。

/// 充电时长上限（分钟）。
pub const MAX_CHARGE_MINUTES: u16 = 4320;
/// 方案时长下限（分钟）。
pub const MIN_SCHEME_MINUTES: u16 = 60;
/// 电量套餐区间（Wh；等价 1..=65 kWh）。
pub const ENERGY_PACKAGE_WH: std::ops::RangeInclusive<u32> = 1_000..=65_000;
/// 电量套餐 kWh 下限 / 上限。
pub const MIN_ENERGY_PACKAGE_KWH: u16 = 1;
pub const MAX_ENERGY_PACKAGE_KWH: u16 = 65;
/// 计费方案浮动秒数上限。
pub const MAX_FLOAT_SECONDS: u16 = 10_800;
/// 计费方案浮动功率上限（0.1W）。
pub const MAX_FLOAT_POWER_DECI_WATTS: u16 = 500;
/// 停机宽限秒数上限。
pub const MAX_STOP_GRACE_SECONDS: i32 = 3600;
/// 损耗率上限（万分比）。
pub const MAX_LOSS_RATE_BP: i32 = 100_000;
/// 报价时长上限（分钟）。
pub const MAX_ESTIMATE_MINUTES: u16 = 600;
/// 报价电量上限（Wh）。
pub const MAX_ESTIMATE_ENERGY_WH: i64 = 100_000;
/// 后台口令长度（字节）。
pub const PASSWORD_LEN: std::ops::RangeInclusive<usize> = 12..=72;
/// 用户充值金额区间（分；下界 100 是充值规则，与退款下界是两条不同规则）。
pub const RECHARGE_AMOUNT_CENTS: std::ops::RangeInclusive<i64> = 100..=5_000_000;
/// 退款金额下界（分；与充值下界不同，禁止混用）。
pub const REFUND_AMOUNT_MIN_CENTS: i64 = 1;
/// 过程查询游标分页 limit。
pub const CURSOR_LIMIT: std::ops::RangeInclusive<u64> = 1..=2_000;
/// 过程查询游标分页默认 limit。
pub const CURSOR_LIMIT_DEFAULT: u64 = 1_000;
/// C 端分页页码上界。
pub const USER_PAGE_MAX: i64 = 100_000;
/// 后台分页页码上界（宽于 C 端）。
pub const ADMIN_PAGE_MAX: i64 = 1_000_000;
/// 分页页长上限（双轨共用）。
pub const PAGE_SIZE_MAX: i64 = 100;