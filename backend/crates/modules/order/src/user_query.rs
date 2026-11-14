//! 用户端充电查询（旧版 internal/order/query、internal/charging/query 组合对齐）：
//! 历史分页、订单详情、充电中订单、实时快照事实与曲线读数。
//!
//! 契约要点（与 admin_query.rs 同约定）：
//! - 归属隔离在 SQL 条件内（user_id 过滤），越权与缺失同归 NotFound，不泄漏他人订单。
//! - 金额 i64 分；charged_kwh 保持 DECIMAL 文本；ENUM 读侧 CAST AS CHAR；
//!   时间输出 `DATE_FORMAT(col, '%Y-%m-%dT%H:%i:%s.%f+00:00')`。
//! - 确认费用在 charge_fee_receipt.result_json 顶层（旧版 ChargeFeeRecord.Fees），
//!   缺失或解码失败按未计费输出 null，不用零值冒充。
//! - billing_status 无计费任务输出空串（旧版契约：空串原样出现，不得替换为 null）。
//! - 实时估费（旧版 LiveService.Fill）未在新仓库接线：充电中输出
//!   live_unavailable 提示（旧版依赖缺失同分支文案），不产生估算费用。

use chargepilot_domain::order::ChargeOrderStatus;
use serde_json::{Value, json};
use sqlx::{MySqlPool, query_as, query_scalar};
use std::collections::HashMap;

/// datetime(3) 列统一输出的 UTC 文本格式（新仓库约定）。
pub const TS_FORMAT: &str = "%Y-%m-%dT%H:%i:%s.%f+00:00";

/// 按列名生成 DATE_FORMAT 表达式。
fn df(col: &str) -> String {
    format!("DATE_FORMAT({col}, '{TS_FORMAT}')")
}

// ---------------------------------------------------------------------------
// 订单行（归属解析与充电中查询）
// ---------------------------------------------------------------------------

/// 已授权订单记录（旧版 ChargeOrderRecord 的查询侧字段）。
#[derive(Debug, Clone)]
pub struct OrderRecord {
    /// 充电订单主键。
    pub id: u64,
    /// 业务订单号。
    pub order_no: String,
    /// 设备编号。
    pub device_id: String,
    /// 端口号（从 1 开始）。
    pub port_no: u8,
    /// 订单状态（charge_order.status 词表）。
    pub status: ChargeOrderStatus,
    /// 业务启动时间（DATE_FORMAT 文本，未开始 None）。
    pub started_at: Option<String>,
    /// 业务结束时间。
    pub ended_at: Option<String>,
    /// 累计电量 DECIMAL 文本（未结束或未确认 None）。
    pub charged_kwh: Option<String>,
    /// 累计秒数。
    pub charged_seconds: Option<u32>,
    /// 优惠抵扣，分。
    pub discount_cents: i64,
    /// 关联支付单主键。
    pub payment_order_id: Option<u64>,
}

const ORDER_RECORD_COLS: &str = "`id`, `order_no`, `device_id`, `port_no`, CAST(`status` AS CHAR), \
         DATE_FORMAT(`started_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
         DATE_FORMAT(`ended_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
         CAST(`charged_kwh` AS CHAR), `charged_seconds`, `discount_cents`, `payment_order_id`";

type OrderRow = (
    u64,
    String,
    String,
    u8,
    ChargeOrderStatus,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    i64,
    Option<u64>,
);

fn to_record(r: OrderRow) -> OrderRecord {
    OrderRecord {
        id: r.0,
        order_no: r.1,
        device_id: r.2,
        port_no: r.3,
        status: r.4,
        started_at: r.5,
        ended_at: r.6,
        charged_kwh: r.7,
        charged_seconds: r.8,
        discount_cents: r.9,
        payment_order_id: r.10,
    }
}

/// 按 user_id 与业务单号解析未删除订单；缺失返回 None（旧版 Owned：越权与缺失同归缺失）。
/// 单号空白或超长按输入错误处理（旧版 Resolve 先校验再查询）。
pub async fn resolve_owned(
    pool: &MySqlPool,
    user_id: u64,
    raw: &str,
) -> Result<Option<OrderRecord>, sqlx::Error> {
    let no = raw.trim();
    if no.is_empty() || no.len() > 64 {
        return Ok(None);
    }
    let sql = format!(
        "SELECT {ORDER_RECORD_COLS} FROM `charge_order` \
         WHERE `order_no` = ? AND `user_id` = ? AND `deleted_at` IS NULL LIMIT 1"
    );
    query_as::<_, OrderRow>(&sql)
        .bind(no)
        .bind(user_id as i64)
        .fetch_optional(pool)
        .await
        .map(|r| r.map(to_record))
}

/// 读取用户最新 paid/charging 未删除订单（旧版 Ongoing：ORDER BY id DESC 取一条）。
pub async fn ongoing_order(
    pool: &MySqlPool,
    user_id: u64,
) -> Result<Option<OrderRecord>, sqlx::Error> {
    let sql = format!(
        "SELECT {ORDER_RECORD_COLS} FROM `charge_order` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL \
           AND CAST(`status` AS CHAR) IN ('paid', 'charging') \
         ORDER BY `id` DESC LIMIT 1"
    );
    query_as::<_, OrderRow>(&sql)
        .bind(user_id as i64)
        .fetch_optional(pool)
        .await
        .map(|r| r.map(to_record))
}

// ---------------------------------------------------------------------------
// 确认费用（charge_fee_receipt.result_json 顶层，旧版 QueryFeeRepo.Fees）
// ---------------------------------------------------------------------------

/// 已确认费用（旧版 FeeFacts；缺失表示尚未计费）。
#[derive(Debug, Clone, Copy)]
pub struct FeeFacts {
    /// 已确认电费，分。
    pub electric_cents: i64,
    /// 已确认服务费，分。
    pub service_cents: i64,
    /// 已确认总费，分。
    pub total_cents: i64,
    /// 结算资金缺口，分。
    pub shortfall_cents: i64,
}

/// 批量读取订单费用回执；缺失或 result_json 非法的订单不进入映射（旧版同语义）。
pub async fn fee_facts(
    pool: &MySqlPool,
    ids: &[u64],
) -> Result<HashMap<u64, FeeFacts>, sqlx::Error> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT `charge_order_id`, CAST(`result_json` AS CHAR), `shortfall_cents` \
         FROM `charge_fee_receipt` WHERE `charge_order_id` IN ",
    );
    qb.push_tuples(ids.iter().copied(), |mut b, id| {
        b.push_bind(id as i64);
    });
    let rows: Vec<(u64, String, i64)> = qb.build_query_as().fetch_all(pool).await?;
    for (id, result_json, shortfall) in rows {
        // 旧版 Fees()：解码失败按不可用跳过，不冒充零值。
        let Ok(v) = serde_json::from_str::<Value>(&result_json) else {
            continue;
        };
        let (Some(e), Some(s), Some(t)) = (
            v["electric_cents"].as_i64(),
            v["service_cents"].as_i64(),
            v["total_cents"].as_i64(),
        ) else {
            continue;
        };
        out.insert(
            id,
            FeeFacts {
                electric_cents: e,
                service_cents: s,
                total_cents: t,
                shortfall_cents: shortfall,
            },
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 历史分页（旧版 ListUserOrderHistory + 费用回填）
// ---------------------------------------------------------------------------

/// 用户历史订单分页：先计总数，再按主键倒序取当页并回填确认费用。
/// 返回旧版 UserOrderHistoryRow 形状的 JSON 行（未计费金额为 null）。
pub async fn list_history(
    pool: &MySqlPool,
    user_id: u64,
    status: Option<&str>,
    page: i64,
    page_size: i64,
) -> Result<(Vec<Value>, i64), sqlx::Error> {
    let total: i64 = query_scalar(
        "SELECT COUNT(*) FROM `charge_order` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL \
         AND (? IS NULL OR CAST(`status` AS CHAR) = ?)",
    )
    .bind(user_id as i64)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    if total == 0 {
        return Ok((Vec::new(), 0));
    }
    let sql = format!(
        "SELECT `id`, `order_no`, `device_id`, `port_no`, CAST(`status` AS CHAR), \
                {} AS `started_at`, {} AS `ended_at`, \
                CAST(`charged_kwh` AS CHAR), `charged_seconds`, `discount_cents` \
         FROM `charge_order` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL \
         AND (? IS NULL OR CAST(`status` AS CHAR) = ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
        df("`started_at`"),
        df("`ended_at`"),
    );
    // 订单列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type OrderListRow = (
        u64,
        String,
        String,
        u8,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<u32>,
        i64,
    );
    let rows: Vec<OrderListRow> = query_as(&sql)
        .bind(user_id as i64)
        .bind(status)
        .bind(status)
        .bind(page_size)
        .bind((page - 1) * page_size)
        .fetch_all(pool)
        .await?;
    let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
    let fees = fee_facts(pool, &ids).await?;
    let items = rows
        .into_iter()
        .map(|r| {
            let fee = fees.get(&r.0);
            json!({
                "order_id": r.0,
                "order_no": r.1,
                "device_id": r.2,
                "port_no": r.3,
                "status": r.4,
                "started_at": r.5,
                "ended_at": r.6,
                "charged_kwh": r.7,
                "charged_seconds": r.8,
                "electric_cents": fee.map(|f| f.electric_cents),
                "service_cents": fee.map(|f| f.service_cents),
                "total_cents": fee.map(|f| f.total_cents),
                "discount_cents": r.9,
            })
        })
        .collect();
    Ok((items, total))
}

// ---------------------------------------------------------------------------
// 详情组合事实（旧版 UserStateRepo.Frozen / Funds）
// ---------------------------------------------------------------------------

/// 计费任务状态（charge_billing_job.status；无任务为空串——旧版契约原样输出）。
pub async fn billing_status(pool: &MySqlPool, order_id: u64) -> Result<String, sqlx::Error> {
    let status: Option<String> = query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `charge_billing_job` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(status.unwrap_or_default())
}

/// 订单刷卡操作数组（旧版 ListOrderOperations：按创建时间升序，无操作为空数组）。
pub async fn card_operations(pool: &MySqlPool, order_id: u64) -> Result<Vec<Value>, sqlx::Error> {
    // 刷卡操作投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type CardOpRow = (String, String, String, u64, u64, String, i64, u16, String);
    let rows: Vec<CardOpRow> = query_as(
        "SELECT CAST(`operation_id` AS CHAR), CAST(`device_id` AS CHAR), CAST(`event_id` AS CHAR), \
                `charge_order_id`, `card_id`, CAST(`kind` AS CHAR), `price_cents`, `minutes`, \
                CAST(`status` AS CHAR) \
         FROM `card_operation` WHERE `charge_order_id` = ? ORDER BY `created_at`",
    )
    .bind(order_id as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(op, device, event, charge_order_id, card_id, kind, price, minutes, status)| {
                json!({
                    "operation_id": op,
                    "device_id": device,
                    "event_id": event,
                    "charge_order_id": charge_order_id,
                    "card_id": card_id,
                    "kind": kind,
                    "price_cents": price,
                    "minutes": minutes,
                    "status": status,
                })
            },
        )
        .collect())
}

/// 首次计费截止（旧版 BillingCutoff：无记录 cutoff_at 为 null、reason 为空串）。
pub async fn billing_cutoff(pool: &MySqlPool, order_id: u64) -> Result<Value, sqlx::Error> {
    let row: Option<(Option<String>, String)> = query_as(&format!(
        "SELECT {} AS `cutoff_at`, `reason` FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
        df("`cutoff_at`")
    ))
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?;
    let (cutoff_at, reason) = row.unwrap_or((None, String::new()));
    Ok(json!({"cutoff_at": cutoff_at, "reason": reason}))
}

/// 从冻结快照解析展示事实（旧版 base 的 frozen 解析）：
/// (display, offer, tariff, rule_description)。
/// 快照缺失或 scheme 不在时返回全 None（对应旧版字段整体省略）。
/// 新仓库 Scheme.display 为文案行结构（旧版 show_* 布尔开关已被新结构替代，
/// tariff 改为 amount 存在即输出、rule_description 输出 remark 原文）。
pub fn frozen_display(
    raw: Option<&str>,
) -> (Option<Value>, Option<Value>, Option<Value>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None, None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return (None, None, None, None);
    };
    // 旧版解析路径：rule.spec.scheme；offer 与 scheme 并列在快照顶层。
    let scheme = v["rule"]["spec"]["scheme"].clone();
    if scheme.is_null() {
        return (None, None, None, None);
    }
    let display = scheme.get("display").cloned().filter(|d| !d.is_null());
    let offer = v.get("offer").cloned();
    let tariff = scheme.get("amount").cloned().filter(|a| !a.is_null());
    let remark = scheme
        .get("remark")
        .and_then(|r| r.as_str())
        .map(str::to_string);
    (display, offer, tariff, remark)
}

/// 订单冻结快照原文（charge_order_pricing.pricing_snapshot；无快照 None）。
pub async fn frozen_snapshot(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let raw: Option<String> = query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(raw)
}

/// 详情支付摘要（旧版 FindOrderPaymentBrief：缺失支付单按无支付处理，不是数据库故障）。
pub async fn payment_brief(
    pool: &MySqlPool,
    payment_order_id: u64,
) -> Result<Option<Value>, sqlx::Error> {
    let row: Option<(String, i64, i64, i64, String)> = query_as(
        "SELECT `order_no`, `total_cents`, `paid_cents`, `refunded_cents`, CAST(`status` AS CHAR) \
         FROM `payment_order` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(payment_order_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(order_no, total, paid, refunded, status)| {
        json!({
            "order_no": order_no,
            "total_cents": total,
            "paid_cents": paid,
            "refunded_cents": refunded,
            "status": status,
        })
    }))
}

/// 订单退款列表（旧版 ListOrderRefunds：按支付业务过滤，倒序，无记录空数组）。
pub async fn order_refunds(pool: &MySqlPool, order_id: u64) -> Result<Vec<Value>, sqlx::Error> {
    // 退款列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type RefundListRow = (String, i64, String, Option<String>, Option<String>);
    let rows: Vec<RefundListRow> = query_as(&format!(
        "SELECT `refund_no`, `refund_cents`, CAST(`status` AS CHAR), `reason`, {} AS `created_at` \
         FROM `refund_record` \
         WHERE `biz_type` = 'charge' AND `biz_id` = ? AND `deleted_at` IS NULL \
         ORDER BY `id` DESC",
        df("`created_at`")
    ))
    .bind(order_id as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(refund_no, cents, status, reason, created_at)| {
            json!({
                "refund_no": refund_no,
                "refund_cents": cents,
                "status": status,
                "reason": reason,
                "created_at": created_at,
            })
        })
        .collect())
}

/// 组装详情/快照公共字段（旧版 UserService.base）。
/// 费用阶段失败以 Err(sqlx) 上抛，由 HTTP 层映射 503。
pub async fn base_view(pool: &MySqlPool, row: &OrderRecord) -> Result<Value, sqlx::Error> {
    let mut out = json!({
        "order_no": row.order_no,
        "status": row.status,
        "device_id": row.device_id,
        "port_no": row.port_no,
        "started_at": row.started_at,
        "ended_at": row.ended_at,
        "charged_kwh": row.charged_kwh,
        "charged_seconds": row.charged_seconds,
        "discount_cents": row.discount_cents,
        "billing_status": billing_status(pool, row.id).await?,
        "card_operations": card_operations(pool, row.id).await?,
        "start_confirmation_pending": row.status == ChargeOrderStatus::Paid,
        "billing_cutoff": billing_cutoff(pool, row.id).await?,
    });
    if let Some(fee) = fee_facts(pool, &[row.id]).await?.get(&row.id) {
        out["electric_cents"] = json!(fee.electric_cents);
        out["service_cents"] = json!(fee.service_cents);
        out["total_cents"] = json!(fee.total_cents);
        out["shortfall_cents"] = json!(fee.shortfall_cents);
    }
    let snapshot = frozen_snapshot(pool, row.id).await?;
    let (display, offer, tariff, remark) = frozen_display(snapshot.as_deref());
    if let Some(d) = display {
        out["display"] = d;
    }
    if let Some(o) = offer {
        // 有冻结方案时 offer 恒出现（缺失套餐为显式 null，旧版同语义）。
        out["offer"] = o;
    }
    if let Some(t) = tariff {
        out["tariff"] = t;
    }
    if let Some(r) = remark {
        out["rule_description"] = json!(r);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 遥测曲线（telemetry / telemetry_aggregate_*，旧版 gateway readPort 语义）
// ---------------------------------------------------------------------------

/// 公开曲线与单页点数上限（旧版 MaxQueryPoints）。
pub const MAX_QUERY_POINTS: i64 = 2000;

/// 指标行（ts + 指标名 + DECIMAL 文本）。
type Reading = (chrono::DateTime<chrono::Utc>, String, String);

/// 原始遥测读数（旧版 ListRawReadings：指定端口仍包含设备级电压、温度）。
async fn list_raw_readings(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
    limit: i64,
) -> Result<Vec<Reading>, sqlx::Error> {
    query_as(
        "SELECT `ts`, CAST(`metric` AS CHAR), CAST(`value_num` AS CHAR) \
         FROM `telemetry` \
         WHERE `device_id` = ? AND `ts` >= ? AND `ts` <= ? AND `value_num` IS NOT NULL \
         AND (`port_no` = ? OR (`port_no` IS NULL AND CAST(`metric` AS CHAR) IN ('voltage_v', 'temperature_c'))) \
         ORDER BY `ts` DESC LIMIT ?",
    )
    .bind(device_id)
    .bind(from)
    .bind(to)
    .bind(port_no)
    .bind(limit * 8)
    .fetch_all(pool)
    .await
}

/// 聚合读数（旧版 ListBucketedReadings：bucket_start AS ts、avg_value）。
async fn list_bucketed_readings(
    pool: &MySqlPool,
    table: &str,
    device_id: &str,
    port_no: u8,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
    limit: i64,
) -> Result<Vec<Reading>, sqlx::Error> {
    use chrono::Datelike as _;
    let sql = format!(
        "SELECT `bucket_start` AS `ts`, CAST(`metric` AS CHAR), CAST(`avg_value` AS CHAR) \
         FROM `{table}` \
         WHERE `device_id` = ? AND `bucket_start` >= ? AND `bucket_start` <= ? \
         AND `bucket_month` >= ? AND `bucket_month` <= ? \
         AND (`port_no` = ? OR (`port_no` IS NULL AND CAST(`metric` AS CHAR) IN ('voltage_v', 'temperature_c'))) \
         ORDER BY `bucket_start` DESC LIMIT ?",
    );
    let month = |t: chrono::DateTime<chrono::Utc>| {
        let d = t.date_naive();
        chrono::NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
    };
    query_as(&sql)
        .bind(device_id)
        .bind(from)
        .bind(to)
        .bind(month(from))
        .bind(month(to))
        .bind(port_no)
        .bind(limit * 8)
        .fetch_all(pool)
        .await
}

/// 按秒合并读数为曲线点（旧版 readPort：同一秒内多指标合并，最新 limit 个点升序返回）。
fn merge_points(
    rows: Vec<Reading>,
    limit: i64,
) -> Vec<(chrono::DateTime<chrono::Utc>, HashMap<String, f64>)> {
    let mut buckets: HashMap<i64, HashMap<String, f64>> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for (ts, metric, raw) in rows {
        let Ok(value) = raw.parse::<f64>() else {
            continue;
        };
        let key = ts.timestamp();
        let entry = buckets.entry(key).or_insert_with(|| {
            order.push(key);
            HashMap::new()
        });
        entry.insert(metric, value);
    }
    // 倒序读取后整体反转：保留最新的 limit 个点并升序输出。
    order.sort_unstable_by(|a, b| b.cmp(a));
    order
        .into_iter()
        .take(limit as usize)
        .map(|k| {
            let ts = chrono::DateTime::from_timestamp(k, 0).unwrap_or_default();
            (ts, buckets.remove(&k).unwrap_or_default())
        })
        .collect()
}

/// 单点 JSON（旧版 CurvePoint：未上报指标为 null，不补零）。
fn point_json(ts: chrono::DateTime<chrono::Utc>, values: &HashMap<String, f64>) -> Value {
    let metric = |k: &str| values.get(k).copied();
    json!({
        "ts": ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "power_w": metric("power_w"),
        "current_a": metric("current_a"),
        "voltage_v": metric("voltage_v"),
        "temperature_c": metric("temperature_c"),
        "battery_soc": metric("battery_soc"),
        "meter_kwh": metric("meter_kwh"),
    })
}

/// 实时曲线窗口读数（旧版 Curve + chooseSource）：
/// 短窗口读原始表，超过 6 小时读小时聚合，其余读 15 分钟聚合。
/// 返回按时间升序的点数组。
pub async fn window_points(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
    limit: i64,
) -> Result<Vec<Value>, sqlx::Error> {
    let buckets = (to - from).num_seconds();
    let rows = if buckets <= 0 || buckets * 4 <= limit * 8 {
        list_raw_readings(pool, device_id, port_no, from, to, limit).await?
    } else if (to - from) > chrono::Duration::hours(6) {
        list_bucketed_readings(
            pool,
            "telemetry_aggregate_hourly",
            device_id,
            port_no,
            from,
            to,
            limit,
        )
        .await?
    } else {
        list_bucketed_readings(
            pool,
            "telemetry_aggregate_15min",
            device_id,
            port_no,
            from,
            to,
            limit,
        )
        .await?
    };
    Ok(merge_points(rows, limit)
        .into_iter()
        .map(|(ts, v)| point_json(ts, &v))
        .collect())
}

/// 历史曲线（旧版 HistoricalCurve + MetricPeaksInRange）：
/// 按粒度读聚合表，返回 (series, summary, boundary_approximate)。
/// 调用方先完成归属、窗口与点数校验。
pub async fn historical_curve(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
    granularity: &str,
) -> Result<(Vec<Value>, Value, bool), sqlx::Error> {
    use chrono::Datelike as _;
    let (bucket_secs, table) = match granularity {
        "hourly" => (3600i64, "telemetry_aggregate_hourly"),
        _ => (900i64, "telemetry_aggregate_15min"),
    };
    let trunc = |t: chrono::DateTime<chrono::Utc>| {
        chrono::DateTime::from_timestamp(t.timestamp() - t.timestamp().rem_euclid(bucket_secs), 0)
            .unwrap_or(t)
    };
    let start_bucket = trunc(started_at);
    let end_bucket = trunc(ended_at);
    let month = |t: chrono::DateTime<chrono::Utc>| {
        let d = t.date_naive();
        chrono::NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
    };
    let rows: Vec<Reading> = query_as(&format!(
        "SELECT `{bucket_col}` AS `ts`, CAST(`metric` AS CHAR), CAST(`avg_value` AS CHAR) \
         FROM `{table}` \
         WHERE `device_id` = ? AND `port_no` = ? \
         AND `{bucket_col}` >= ? AND `{bucket_col}` <= ? \
         AND `bucket_month` >= ? AND `bucket_month` <= ? \
         ORDER BY `{bucket_col}` DESC LIMIT ?",
        bucket_col = "bucket_start",
    ))
    .bind(device_id)
    .bind(port_no)
    .bind(start_bucket)
    .bind(end_bucket)
    .bind(month(start_bucket))
    .bind(month(end_bucket))
    .bind(MAX_QUERY_POINTS)
    .fetch_all(pool)
    .await?;
    let series: Vec<Value> = merge_points(rows, MAX_QUERY_POINTS)
        .into_iter()
        .map(|(ts, v)| point_json(ts, &v))
        .collect();
    // 摘要（旧版 MetricPeaksInRange）：功率、温度峰值与按样本数加权的均值，DECIMAL 文本。
    let peaks: Vec<(String, String, String)> = query_as(&format!(
        "SELECT CAST(`metric` AS CHAR), CAST(MAX(`max_value`) AS CHAR), \
                CAST(SUM(`avg_value` * `count`) / NULLIF(SUM(`count`), 0) AS CHAR) \
         FROM `{table}` \
         WHERE `device_id` = ? AND `port_no` = ? \
         AND `bucket_start` >= ? AND `bucket_start` <= ? \
         AND `bucket_month` >= ? AND `bucket_month` <= ? \
         AND CAST(`metric` AS CHAR) IN ('power_w', 'temperature_c') \
         GROUP BY `metric`",
    ))
    .bind(device_id)
    .bind(port_no)
    .bind(start_bucket)
    .bind(end_bucket)
    .bind(month(start_bucket))
    .bind(month(end_bucket))
    .fetch_all(pool)
    .await?;
    let mut summary = json!({});
    for (metric, peak, avg) in peaks {
        if metric == "power_w" {
            summary["max_power_w"] = json!(peak);
            summary["avg_power_w"] = json!(avg);
        } else {
            summary["max_temperature_c"] = json!(peak);
        }
    }
    let boundary = started_at != start_bucket || ended_at != end_bucket;
    Ok((series, summary, boundary))
}

// ---------------------------------------------------------------------------
// 快照端点的两分钟窗口观测（旧版 Snapshot 遥测段）
// ---------------------------------------------------------------------------

/// 窗口内最新观测（旧版 Snapshot 循环最后一个有效点的语义）：
/// 返回 (telemetry_at, power_w, voltage_v, temperature_c, meter_kwh, has_value)。
pub async fn latest_observation(
    pool: &MySqlPool,
    device_id: &str,
    port_no: u8,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> Result<
    Option<(
        chrono::DateTime<chrono::Utc>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    )>,
    sqlx::Error,
> {
    let rows = list_raw_readings(pool, device_id, port_no, from, to, MAX_QUERY_POINTS).await?;
    let mut merged = merge_points(rows, MAX_QUERY_POINTS);
    let Some((ts, values)) = merged.pop() else {
        return Ok(None);
    };
    let metric = |k: &str| values.get(k).copied();
    Ok(Some((
        ts,
        metric("power_w"),
        metric("voltage_v"),
        metric("temperature_c"),
        metric("meter_kwh"),
    )))
}
