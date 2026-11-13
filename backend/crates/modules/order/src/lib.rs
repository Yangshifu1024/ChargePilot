//! order 域：充电订单写模型（表归属：charge_order / charge_order_pricing /
//! charge_event_log 归 order；编号规则纯函数在 domain::payment_intent）。
//! 支付成功事务由 payment 模块编排，本模块只提供订单侧的表访问。

use chargepilot_domain::money::PaymentStatus;
use chrono::Datelike;
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::MySqlConnection;

/// 订单状态词表（charge_order.status，与 domain/charging_order 的状态机一致）。
pub struct OrderStatus;
impl OrderStatus {
    pub const PENDING_PAYMENT: &'static str = "pending_payment";
    pub const PAID: &'static str = "paid";
    pub const CHARGING: &'static str = "charging";
    pub const COMPLETED: &'static str = "completed";
    pub const CANCELLED: &'static str = "cancelled";
    pub const FAILED: &'static str = "failed";
    pub const REFUNDING: &'static str = "refunding";
    pub const REFUNDED: &'static str = "refunded";
}

/// 占用端口的订单状态：已支付未起充与充电中（旧版 paidOrChargingOrderStatuses）。
pub const ONGOING_STATUSES: [&str; 2] = [OrderStatus::PAID, OrderStatus::CHARGING];

/// 月分区归属日期：取给定时刻对应月份的第一天（旧版 dbutil.MonthStart）。
/// 分区表主键含 created_month，所有按月分区表的写入都带此值。
pub fn month_start_of(now: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
    let date = now.date_naive();
    chrono::NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

/// 支付成功时创建的充电订单字段（旧版 ChargeOrderRecord 的写入侧）。
#[derive(Debug, Clone)]
pub struct PaidOrderDraft {
    pub order_no: String,
    pub user_id: u64,
    pub device_id: String,
    pub port_no: u8,
    pub port_code: String,
    pub payment_order_id: u64,
    /// 设备协议充电模式码（0 时长 / 1 电量 / 4 金额，由意图冻结）。
    pub charge_mode: u8,
    /// 下发设备的充电数量（分钟或 Wh，单位由 charge_mode 确定）。
    pub charge_quantity: u16,
    pub discount_cents: i64,
}

/// 订单域错误（支付成功事务中的冲突语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderError {
    /// 业务单号冲突（同秒同设备同端口重复或编号超限）。
    NumberConflict,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::NumberConflict => f.write_str("充电单号冲突"),
            OrderError::Storage(m) => write!(f, "订单存储错误: {m}"),
        }
    }
}

impl std::error::Error for OrderError {}

impl From<sqlx::Error> for OrderError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            // 23000（MySQL 1062）：order_no 唯一键冲突（uk_order_no 按分区键组合唯一）。
            if db.code().as_deref() == Some("23000") {
                return OrderError::NumberConflict;
            }
        }
        OrderError::Storage(e.to_string())
    }
}

/// 生成并查重充电业务单号（旧版 NewChargeOrderNumber）。
/// 同秒同设备同端口重复时报冲突，由调用方整体回滚重试。
pub async fn new_charge_order_no_tx(
    tx: &mut MySqlConnection,
    at_unix_micros: i64,
    device: &str,
    port_no: u8,
) -> Result<String, OrderError> {
    let no = chargepilot_domain::payment_intent::charge_order_no(at_unix_micros, device, port_no)
        .map_err(|_| OrderError::NumberConflict)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_order` WHERE `order_no` = ? AND `created_month` = ?",
    )
    .bind(&no)
    .bind(month_start_of(chrono::Utc::now()))
    .fetch_one(&mut *tx)
    .await?;
    if count != 0 {
        return Err(OrderError::NumberConflict);
    }
    Ok(no)
}

/// 在事务内创建已支付充电订单，返回订单主键（旧版 CreatePaidInTx）。
/// 分区表主键含 created_month，插入必须带月首日期。
pub async fn create_paid_in_tx(
    tx: &mut MySqlConnection,
    draft: &PaidOrderDraft,
) -> Result<u64, OrderError> {
    let month = month_start_of(chrono::Utc::now());
    let id: u64 = sqlx::query(
        "INSERT INTO `charge_order` \
         (`order_no`, `user_id`, `device_id`, `port_no`, `port_code`, `payment_order_id`, \
          `status`, `payment_status`, `charge_mode`, `charge_quantity`, `discount_cents`, `created_month`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&draft.order_no)
    .bind(draft.user_id as i64)
    .bind(&draft.device_id)
    .bind(draft.port_no)
    .bind(&draft.port_code)
    .bind(draft.payment_order_id as i64)
    .bind(OrderStatus::PAID)
    .bind(PaymentStatus::PAID)
    .bind(draft.charge_mode)
    .bind(draft.charge_quantity)
    .bind(draft.discount_cents)
    .bind(month)
    .execute(&mut *tx)
    .await?
    .last_insert_id() as u64;
    Ok(id)
}

/// 冻结计费方案快照到订单（旧版 FreezePricingInTx）。
pub async fn freeze_pricing_in_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    payment_intent_id: &str,
    user_id: u64,
    port_code: &str,
    pricing_snapshot: &serde_json::Value,
) -> Result<(), OrderError> {
    sqlx::query(
        "INSERT INTO `charge_order_pricing` \
         (`charge_order_id`, `payment_intent_id`, `user_id`, `port_code`, `pricing_snapshot`) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(payment_intent_id)
    .bind(user_id as i64)
    .bind(port_code)
    .bind(pricing_snapshot)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 追加订单事件（旧版 AppendEventInTx）；event_id 唯一，重复即冲突。
pub async fn append_event_in_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    event_id: &str,
    event: &str,
    actor: &str,
    detail: &str,
    occurred_at_unix_micros: i64,
) -> Result<(), OrderError> {
    let occurred = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(occurred_at_unix_micros)
        .unwrap_or_else(chrono::Utc::now);
    sqlx::query(
        "INSERT INTO `charge_event_log` \
         (`charge_order_id`, `event_id`, `event`, `actor`, `detail`, `occurred_at`) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(event_id)
    .bind(event)
    .bind(actor)
    .bind(detail)
    .bind(occurred)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 写入领域事件出站表（进程内投递；旧版 ops.AppendEventInTx 的新版对应物）。
pub async fn append_domain_event_tx(
    tx: &mut MySqlConnection,
    event_id: &str,
    source: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), OrderError> {
    sqlx::query(
        "INSERT INTO `domain_event` (`event_id`, `source`, `kind`, `payload_json`) VALUES (?, ?, ?, ?)",
    )
    .bind(event_id)
    .bind(source)
    .bind(kind)
    .bind(payload)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 支付单号：P + 号段分配 ID（旧版 NewOrderNumber；新版号段替代旧雪花表）。
pub async fn new_payment_order_no(allocator: &SegmentAllocator) -> Result<String, OrderError> {
    let id = allocator
        .next_id()
        .await
        .map_err(|e| OrderError::Storage(e.to_string()))?;
    Ok(format!("P{id}"))
}
