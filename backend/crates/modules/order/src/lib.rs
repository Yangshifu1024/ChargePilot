//! order 域：充电订单写模型（表归属：charge_order / charge_order_pricing /
//! charge_event_log 归 order；编号规则纯函数在 domain::payment_intent）。
//! 支付成功事务由 payment 模块编排，本模块只提供订单侧的表访问。

pub mod admin_query;
pub mod feedback;

use chargepilot_domain::money::PaymentStatus;
use chargepilot_platform::ids::SegmentAllocator;
use chrono::Datelike;
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

/// 生命周期推进所需的订单行锁视图（旧版 LifecycleView + LockForLifecycleInTx）。
#[derive(Debug, Clone)]
pub struct LifecycleView {
    pub charge_order_id: u64,
    pub order_no: String,
    pub user_id: u64,
    pub device_id: String,
    pub port_no: u8,
    pub status: String,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 锁订单行并读取生命周期推进所需事实；不存在返回 None（旧版 LockForLifecycleInTx）。
pub async fn lock_for_lifecycle_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    order_no: &str,
) -> Result<Option<LifecycleView>, OrderError> {
    let row: Option<(
        u64,
        String,
        u64,
        String,
        u8,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT `id`, `order_no`, `user_id`, `device_id`, `port_no`, `status`, \
                    `started_at`, `ended_at` \
             FROM `charge_order` WHERE `id` = ? AND `order_no` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .bind(order_no)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row.map(
        |(id, no, user_id, device_id, port_no, status, started_at, ended_at)| LifecycleView {
            charge_order_id: id,
            order_no: no,
            user_id,
            device_id,
            port_no,
            status,
            started_at,
            ended_at,
        },
    ))
}

/// 启动成功：paid → charging 并写业务启动时间（旧版 StartInTx 的状态推进）。
/// 条件更新保证只在 paid 状态推进一次；RowsAffected==0 视为冲突。
pub async fn mark_charging_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    order_no: &str,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), OrderError> {
    let n = sqlx::query(
        "UPDATE `charge_order` SET `status` = ?, `started_at` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ?",
    )
    .bind(OrderStatus::CHARGING)
    .bind(started_at)
    .bind(charge_order_id as i64)
    .bind(order_no)
    .bind(OrderStatus::PAID)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(OrderError::NumberConflict);
    }
    Ok(())
}

/// 启动被拒（设备明确拒绝或超时兜底）：paid → refunding 并记录原因。
/// 旧版 MarkStartRejectedInTx 语义——订单进入退款流程，渠道退款由执行器
/// 按 refund_record 推进，成功收尾 refunding → refunded；failed 保持死值
/// （state-machine.md 定案，旧版逐写点核实：生产零写入）。
pub async fn mark_start_rejected_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    order_no: &str,
    reason: &str,
) -> Result<(), OrderError> {
    // 类型层迁移证明：paid 证据 → mark_start_rejected → refunding。
    let _refunding = chargepilot_domain::charging_state::PaidOrder {
        id: chargepilot_domain::charging_state::OrderId(charge_order_id),
    }
    .mark_start_rejected(reason);
    let n = sqlx::query(
        "UPDATE `charge_order` SET `status` = ?, `failure_reason` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ?",
    )
    .bind(OrderStatus::REFUNDING)
    .bind(reason)
    .bind(charge_order_id as i64)
    .bind(order_no)
    .bind(OrderStatus::PAID)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(OrderError::NumberConflict);
    }
    Ok(())
}

/// 结束回执：charging → completed 并写计量列（旧版 CompleteInTx）。
/// 电量文本由调用方按 Wh→kWh 四位小数换算（StringFixed(4) 语义）。
#[allow(clippy::too_many_arguments)]
pub async fn complete_tx(
    tx: &mut MySqlConnection,
    charge_order_id: u64,
    order_no: &str,
    ended_at: chrono::DateTime<chrono::Utc>,
    charged_kwh: &str,
    charged_seconds: i64,
) -> Result<(), OrderError> {
    let n = sqlx::query(
        "UPDATE `charge_order` SET `status` = ?, `ended_at` = ?, \
         `charged_kwh` = ?, `charged_seconds` = ? \
         WHERE `id` = ? AND `order_no` = ? AND `status` = ? AND `ended_at` IS NULL",
    )
    .bind(OrderStatus::COMPLETED)
    .bind(ended_at)
    .bind(charged_kwh)
    .bind(charged_seconds)
    .bind(charge_order_id as i64)
    .bind(order_no)
    .bind(OrderStatus::CHARGING)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(OrderError::NumberConflict);
    }
    Ok(())
}

/// 预占端口（旧版 active_port_charge 写入）；重复预占冲突由主键仲裁。
pub async fn occupy_port_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    port_no: u8,
    charge_order_id: u64,
    user_id: u64,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), OrderError> {
    // port_id 复合编码：device 哈希无法逆，直接用 (device_id,port_no) 唯一键并令
    // port_id 取 charge_order_id（同单同端口一一对应，旧版为 gateway 端口主键）。
    sqlx::query(
        "INSERT INTO `active_port_charge` \
         (`port_id`, `device_id`, `port_no`, `charge_order_id`, `user_id`, `started_at`) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(charge_order_id as i64)
    .bind(device_id)
    .bind(port_no)
    .bind(charge_order_id as i64)
    .bind(user_id as i64)
    .bind(started_at)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 释放端口占用：必须匹配订单（迟到回执不能释放新占用）；返回是否删除了行。
pub async fn release_port_tx(
    tx: &mut MySqlConnection,
    device_id: &str,
    port_no: u8,
    charge_order_id: u64,
) -> Result<bool, OrderError> {
    let n = sqlx::query(
        "DELETE FROM `active_port_charge` \
         WHERE `device_id` = ? AND `port_no` = ? AND `charge_order_id` = ?",
    )
    .bind(device_id)
    .bind(port_no)
    .bind(charge_order_id as i64)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}
