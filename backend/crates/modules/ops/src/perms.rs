//! 后台权限码词表（profile.permissions 的单一真相）：模块自身即清单——
//! 新增权限码在此登记，判定点经常量引用防拼写漂移（架构评审遗留 B2）。
//! 角色模板与数据库种子里的权限码字面量不在本清单（初始化数据，非代码判定）。

/// 后台权限码（命名：点分转大写下划线）。
pub const ADMIN_USER_CREATE: &str = "admin_user.create";
pub const ADMIN_USER_DELETE: &str = "admin_user.delete";
pub const ADMIN_USER_READ: &str = "admin_user.read";
pub const ADMIN_USER_RESET_PASSWORD: &str = "admin_user.reset_password";
pub const ADMIN_USER_UPDATE: &str = "admin_user.update";
pub const ALERT_ACK: &str = "alert.ack";
pub const ALERT_READ: &str = "alert.read";
pub const ANNOUNCEMENT_CREATE: &str = "announcement.create";
pub const ANNOUNCEMENT_READ: &str = "announcement.read";
pub const AUDIT_READ: &str = "audit.read";
pub const BILLING_METER_REVIEW: &str = "billing.meter.review";
pub const COUPON_ACTIVITY_MANAGE: &str = "coupon.activity.manage";
pub const COUPON_ACTIVITY_READ: &str = "coupon.activity.read";
pub const COUPON_CREATE: &str = "coupon.create";
pub const COUPON_GRANT: &str = "coupon.grant";
pub const COUPON_READ: &str = "coupon.read";
pub const COUPON_UPDATE: &str = "coupon.update";
pub const DASHBOARD_READ: &str = "dashboard.read";
pub const DEVICE_IMPORT: &str = "device.import";
pub const DEVICE_OPERATE: &str = "device.operate";
pub const DEVICE_READ: &str = "device.read";
pub const FAULT_DISPATCH: &str = "fault.dispatch";
pub const FAULT_READ: &str = "fault.read";
pub const FAULT_RESOLVE: &str = "fault.resolve";
pub const FEEDBACK_READ: &str = "feedback.read";
pub const FEEDBACK_REPLY: &str = "feedback.reply";
pub const FINANCE_READ: &str = "finance.read";
pub const FINANCE_REFUND_READ: &str = "finance.refund.read";
pub const FINANCE_REFUND_RETRY: &str = "finance.refund.retry";
pub const FINANCE_WALLET_RISK_RELEASE: &str = "finance.wallet_risk.release";
pub const FINANCE_WALLET_RISK_REVIEW: &str = "finance.wallet_risk.review";
pub const INVOICE_REVIEW: &str = "invoice.review";
pub const ONLINE_CARD_MANAGE: &str = "online_card.manage";
pub const ORDER_READ: &str = "order.read";
pub const ORDER_REFUND_CREATE: &str = "order.refund.create";
pub const ORDER_REFUND_REVIEW: &str = "order.refund.review";
pub const PRICING_READ: &str = "pricing.read";
pub const PRICING_RULE_CREATE: &str = "pricing.rule.create";
pub const PRICING_RULE_UPDATE: &str = "pricing.rule.update";
pub const ROLE_CREATE: &str = "role.create";
pub const ROLE_DELETE: &str = "role.delete";
pub const ROLE_UPDATE: &str = "role.update";
pub const STATION_CREATE: &str = "station.create";
pub const STATION_READ: &str = "station.read";
pub const STATION_UPDATE: &str = "station.update";
pub const USER_READ: &str = "user.read";
pub const VENDOR_CREATE: &str = "vendor.create";
pub const VENDOR_READ: &str = "vendor.read";
pub const VENDOR_UPDATE: &str = "vendor.update";
