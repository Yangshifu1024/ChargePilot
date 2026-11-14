//! 业务界限值单一真相源（二期票 15）。
//!
//! 每条常量注明旧版出处；旧版多为多处内联、无具名常量，标注「旧版多处内联」。
//! 分页双轨（C 端 10 万 / 后台 100 万）是旧版有意设计（`httpapi/gin.go`
//! DefaultPageLimit 与 `admin/request.go` MaxAdminPageLimit 分别配置），不是漂移。

/// 充电时长上限（分钟）。旧版多处内联：offer.go / spec.go / device.go /
/// capabilities.go / scheme.go。
pub const MAX_CHARGE_MINUTES: u16 = 4320;
/// 方案时长下限（分钟，旧版 scheme.go 同函数的 `>= 60`）。
pub const MIN_SCHEME_MINUTES: u16 = 60;
/// 电量套餐区间（Wh；等价旧版 scheme.go 的 1..=65 kWh）。
pub const ENERGY_PACKAGE_WH: std::ops::RangeInclusive<u32> = 1_000..=65_000;
/// 电量套餐 kWh 下限 / 上限（旧版 scheme.go `1..=65`）。
pub const MIN_ENERGY_PACKAGE_KWH: u16 = 1;
pub const MAX_ENERGY_PACKAGE_KWH: u16 = 65;
/// 计费方案浮动秒数上限（旧版 scheme.go 同函数）。
pub const MAX_FLOAT_SECONDS: u16 = 10_800;
/// 计费方案浮动功率上限（0.1W，旧版 scheme.go 同函数）。
pub const MAX_FLOAT_POWER_DECI_WATTS: u16 = 500;
/// 停机宽限秒数上限（旧版 spec.go）。
pub const MAX_STOP_GRACE_SECONDS: i32 = 3600;
/// 损耗率上限（万分比，旧版 spec.go）。
pub const MAX_LOSS_RATE_BP: i32 = 100_000;
/// 报价时长上限（分钟，旧版 estimate.go）。
pub const MAX_ESTIMATE_MINUTES: u16 = 600;
/// 报价电量上限（Wh，旧版 estimate.go `1..=100000`）。
pub const MAX_ESTIMATE_ENERGY_WH: i64 = 100_000;
/// 后台口令长度（字节；旧版 ops 同语义）。
pub const PASSWORD_LEN: std::ops::RangeInclusive<usize> = 12..=72;
/// 用户充值金额区间（分；下界 100 是充值规则，与退款下界是两条不同规则）。
pub const RECHARGE_AMOUNT_CENTS: std::ops::RangeInclusive<i64> = 100..=5_000_000;
/// 退款金额下界（分；旧版 wallet/refund.go 与充值下界不同，禁止混用）。
pub const REFUND_AMOUNT_MIN_CENTS: i64 = 1;
/// 过程查询游标分页 limit（旧版 admin 订单过程接口）。
pub const CURSOR_LIMIT: std::ops::RangeInclusive<u64> = 1..=2_000;
/// 过程查询游标分页默认 limit。
pub const CURSOR_LIMIT_DEFAULT: u64 = 1_000;
/// C 端分页上限（页码；旧版 httpapi/gin.go DefaultPageLimit）。
pub const USER_PAGE_MAX: i64 = 100_000;
/// 后台分页上限（页码；旧版 admin/request.go MaxAdminPageLimit，有意宽于 C 端）。
pub const ADMIN_PAGE_MAX: i64 = 1_000_000;
/// 分页页长上限（双轨共用）。
pub const PAGE_SIZE_MAX: i64 = 100;
