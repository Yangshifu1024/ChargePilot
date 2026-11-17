//! 后台订单与支付单只读查询：列表、详情、时间线、过程采样与支付单视图。
//!
//! 契约要点：
//! - 响应 JSON 字段名、可空性与单位与对外契约一致
//! （金额单位分、meter_kwh 保持 DECIMAL 文本、user_id 十进制字符串）。
//! - 时间输出统一 `DATE_FORMAT(col, '%Y-%m-%dT%H:%i:%s.%f+00:00')`（UTC ISO 微秒格式）。
//! - 读侧对 DECIMAL / ENUM / BINARY / ascii_bin 列一律 `CAST(col AS CHAR)`；
//! BIGINT UNSIGNED 读 u64，金额 i64 分，可空 Option。
//! - 固定 SQL + `(? IS NULL OR col = ?)` 双绑定守卫（与 station.rs 同款），无动态拼接。
//! - 实时计量估算尚未接线：充电中订单输出 live_unavailable 提示，不产生估算费用。

use chargepilot_domain::money::ChargePaymentStatus;
use chargepilot_domain::money::PaymentOrderStatus;
use chargepilot_domain::order::ChargeOrderStatus;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{MySqlPool, query_as, query_scalar};

/// datetime(3) 列统一输出的 UTC 文本格式。
const TS_FORMAT: &str = "%Y-%m-%dT%H:%i:%s.%f+00:00";

/// 按列名生成 DATE_FORMAT 表达式。
fn df(col: &str) -> String {
    format!("DATE_FORMAT({col}, '{TS_FORMAT}')")
}

// ---------------------------------------------------------------------------
// 共享 SQL 片段
// ---------------------------------------------------------------------------

/// 启动来源派生列：卡记录优先，其次余额、扫码支付。
const START_SOURCE_EXPR: &str = "(CASE WHEN EXISTS (SELECT 1 FROM `card_charge` `cc` WHERE `cc`.`charge_order_id` = `c`.`id`) THEN 'card' WHEN `p`.`pay_method` = 'balance' THEN 'balance' WHEN `p`.`pay_method` = 'wechat' THEN 'payment' ELSE NULL END)";

/// 退款进度派生列：在途退款优先，非空保证。
const REFUND_STATUS_EXPR: &str = "(CASE WHEN EXISTS (SELECT 1 FROM `refund_record` `r` WHERE `r`.`payment_order_id` = `p`.`id` AND `r`.`status` IN ('pending','processing') AND `r`.`deleted_at` IS NULL) THEN 'processing' WHEN `c`.`payment_status` = 'refunded' THEN 'refunded' WHEN `c`.`status` = 'refunding' THEN 'processing' WHEN `p`.`refunded_cents` > 0 THEN 'partial_refunded' ELSE 'none' END)";

/// 支付单金额确认状态派生列。
const PAYMENT_STATUS_EXPR: &str = "(CASE WHEN `p`.`paid_cents` > 0 AND `p`.`refunded_cents` >= `p`.`paid_cents` THEN 'refunded' WHEN `p`.`refunded_cents` > 0 THEN 'partial_refunded' WHEN `p`.`paid_cents` > 0 OR `p`.`status` IN ('paid','partial_refunded','refunded') THEN 'paid' ELSE 'pending' END)";

/// 订单、支付与支付意图的联表基础：排除软删除订单与支付。
const ORDER_BASE_FROM: &str = "FROM `charge_order` AS `c` \
         LEFT JOIN `payment_order` AS `p` ON `p`.`id` = `c`.`payment_order_id` AND `p`.`user_id` = `c`.`user_id` AND `p`.`deleted_at` IS NULL \
         LEFT JOIN `charge_payment_intent` AS `i` ON `i`.`charge_order_id` = `c`.`id`";

/// 后台订单视图列。
fn order_select() -> String {
    format!(
        "`c`.`id` AS `order_id`, `c`.`order_no`, CAST(`c`.`user_id` AS CHAR) AS `user_id`, \
         `c`.`device_id`, `c`.`port_no`, `i`.`station_id`, CAST(`c`.`status` AS CHAR) AS `status`, \
         CAST(`c`.`business_status` AS CHAR) AS `business_status`, CAST(`c`.`payment_status` AS CHAR) AS `payment_status`, \
         {START_SOURCE_EXPR} AS `start_source`, \
         {created} AS `created_at`, {started} AS `started_at`, {ended} AS `ended_at`, \
         `c`.`charged_seconds` AS `duration_seconds`, CAST(`c`.`charged_kwh` AS CHAR) AS `meter_kwh`, \
         `c`.`electric_cents` AS `electric_fee_cents`, `c`.`service_cents` AS `service_fee_cents`, `c`.`total_cents` AS `total_fee_cents`, \
         `c`.`payment_order_id`, `p`.`order_no` AS `payment_order_no`, CAST(`p`.`status` AS CHAR) AS `payment_order_status`, \
         `p`.`paid_cents`, `p`.`refunded_cents`, `c`.`failure_reason`, \
         {REFUND_STATUS_EXPR} AS `refund_status`",
        created = df("`c`.`created_at`"),
        started = df("`c`.`started_at`"),
        ended = df("`c`.`ended_at`"),
    )
}

/// 时间区间守卫：启动优先、未启动回退创建时间的闭区间。
/// 无界时绑定为 NULL 使该侧恒真，整体退化为全量，等价于。
const ORDER_TIME_GUARD: &str = " AND ((`c`.`started_at` IS NOT NULL \
         AND (? IS NULL OR `c`.`started_at` >= ?) AND (? IS NULL OR `c`.`started_at` <= ?)) \
         OR (`c`.`started_at` IS NULL \
         AND (? IS NULL OR `c`.`created_at` >= ?) AND (? IS NULL OR `c`.`created_at` <= ?)))";

// ---------------------------------------------------------------------------
// 行模型
// ---------------------------------------------------------------------------

/// 后台订单列表与详情行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct OrderViewRow {
    /// 充电订单主键。
    pub order_id: u64,
    /// 业务订单号，对外展示和排障都用它。
    pub order_no: String,
    /// 下单用户 ID。
    pub user_id: String,
    /// 设备 ID。
    pub device_id: String,
    /// 充电枪序号。
    pub port_no: u8,
    /// 所属站点 ID；关联不上时为 null。
    pub station_id: Option<u64>,
    /// 站点名称；站点缺失或已删除时为 null。
    #[sqlx(skip)]
    pub station_name: Option<String>,
    /// 订单内部状态（charge_order.status 词表）。
    pub status: ChargeOrderStatus,
    /// 持久化业务状态：pending_start/charging/completed（生成列）。
    pub business_status: String,
    /// 持久化支付状态（charge_order.payment_status 词表）。
    pub payment_status: ChargePaymentStatus,
    /// 启动来源 payment/balance/card；缺失来源时为 null。
    pub start_source: Option<String>,
    /// 下单时冻结的计费方案名称；缺失快照或有效名称时为 null。
    #[sqlx(skip)]
    pub selected_scheme_name: Option<String>,
    /// 下单时冻结的套餐名称；缺失有效套餐名称时为 null。
    #[sqlx(skip)]
    pub selected_package_name: Option<String>,
    /// 订单创建时间。
    pub created_at: String,
    /// 实际开始充电时间；未开始为 null。
    pub started_at: Option<String>,
    /// 结束充电时间；未结束为 null。
    pub ended_at: Option<String>,
    /// 计费时长（秒）；计费完成后才有。
    pub duration_seconds: Option<u32>,
    /// 电表读数（kWh）；DECIMAL 文本，保持字符串避免浮点丢精度。
    pub meter_kwh: Option<String>,
    /// 电费（分）；尚未计费为 null。
    pub electric_fee_cents: Option<i64>,
    /// 服务费（分）；尚未计费为 null。
    pub service_fee_cents: Option<i64>,
    /// 应收合计（分）；尚未计费为 null。
    pub total_fee_cents: Option<i64>,
    /// 退款进度 none/processing/partial_refunded/refunded，由 SQL 依据支付单与退款单算出。
    pub refund_status: String,
    /// 关联支付单主键；未发起支付为 null。
    pub payment_order_id: Option<u64>,
    /// 支付单号；未发起支付为 null。
    pub payment_order_no: Option<String>,
    /// 支付单内部流程状态（payment_order.status 词表）；未关联时为 null。
    pub payment_order_status: Option<PaymentOrderStatus>,
    /// 实收金额（分）；未支付为 null。
    pub paid_cents: Option<i64>,
    /// 已成功退款金额（分）；有效支付单未退款为 0，缺失支付单为 null。
    pub refunded_cents: Option<i64>,
    /// 失败原因；非失败单为 null。
    pub failure_reason: Option<String>,
    /// 退款操作人展示字段；仅详情接口按实时权限回填，列表恒 null。
    #[sqlx(skip)]
    pub refund_applicant_id: Option<String>,
    /// 计费标识；列表恒 null，详情为 {"calculation_no": ...}。
    #[sqlx(skip)]
    pub billing: Option<Value>,
    /// 下单时冻结的套餐及规则；列表恒 null，详情按快照解析。
    #[sqlx(skip)]
    pub selected_package: Option<Value>,
    /// 实时读取失败的公开提示。
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_unavailable: Option<String>,
}

/// 订单事件时间线行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct TimelineRow {
    /// 事件唯一标识。
    pub event_id: String,
    /// 发生 UTC 时刻（occurred_at 列）。
    pub at: String,
    /// 事件类型。
    pub event: String,
    /// 执行来源。
    pub actor: String,
    /// 业务说明。
    pub detail: String,
}

/// 充电过程采样点。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct ProcessPointRow {
    /// 过程行主键，游标。
    pub id: u64,
    /// 平台接收时刻（UTC）。
    pub ts: String,
    /// 功率，W。
    pub power_w: f64,
    /// 累计电量，kWh。
    pub charged_kwh: f64,
    /// 剩余电量，kWh。
    pub remaining_kwh: f64,
    /// 累计时长，秒。
    pub charged_seconds: u32,
    /// 剩余时长，秒。
    pub remaining_seconds: u32,
    /// 信号强度，零为有效值。
    pub signal_strength: u8,
    /// 端口状态，未上报为 null。
    pub port_status: Option<u8>,
    /// 电压，V，未上报为 null。
    pub voltage_v: Option<u16>,
    /// 温度，摄氏度，未上报为 null。
    pub temperature_c: Option<i16>,
    /// 设备状态，未上报为 null。
    pub device_status: Option<u8>,
}

/// 后台支付单行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct PaymentOrderRow {
    /// 支付主键。
    pub payment_order_id: u64,
    /// 商户支付单号。
    pub order_no: String,
    /// 付款人。
    pub user_id: String,
    /// charge 充电支付，wallet_recharge 余额充值。
    pub biz_type: String,
    /// wechat 微信支付、balance 余额支付，另含 mixed 与 coupon。
    pub pay_method: String,
    /// 支付单内部流程状态。
    pub status: String,
    /// 金额确认后的 pending/paid/refunded/partial_refunded。
    pub payment_status: String,
    /// 应付金额，分。
    pub total_cents: i64,
    /// 已确认到账金额，分。
    pub paid_cents: i64,
    /// 累计成功退款金额，分。
    pub refunded_cents: i64,
    /// 已确认到账时刻，未到账为 null。
    pub paid_at: Option<String>,
    /// 支付创建时刻。
    pub created_at: String,
    /// 当前有效关联充电主键，充值或无关联为 null。
    pub charge_order_id: Option<u64>,
    /// 当前有效关联充电单号，充值或无关联为 null。
    pub charge_order_no: Option<String>,
}

/// 订单关联退款行（字段取自 refund_record 与 refund_review，按支付单过滤倒序）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct OrderRefundRow {
    /// 退款记录主键。
    pub refund_id: u64,
    /// 退款业务编号。
    pub refund_no: String,
    /// 退款申请人（十进制字符串）。
    pub user_id: String,
    /// 本次退款金额，分。
    pub refund_cents: i64,
    /// 退款状态（refund_record.status 词表）。
    pub status: String,
    /// 退款原因。
    pub reason: Option<String>,
    /// 最近一次执行失败原因。
    pub failure_reason: Option<String>,
    /// 渠道执行策略 manual_review/automatic。
    pub execution_policy: String,
    /// 退款创建时刻。
    pub created_at: String,
    /// 任务完成时刻，未完成为 null。
    pub completed_at: Option<String>,
    /// 一级审核人，未经复核为 null。
    pub review_first_signer: Option<u64>,
    /// 一级审核意见。
    pub review_first_comment: Option<String>,
    /// 二级审核人。
    pub review_second_signer: Option<u64>,
    /// 二级审核意见。
    pub review_second_comment: Option<String>,
    /// 审核通过时刻。
    pub review_approved_at: Option<String>,
}

/// 过程查询的订单身份。
#[derive(Debug, Clone)]
pub struct ProcessIdentity {
    /// 中央订单主键。
    pub id: u64,
    /// 业务订单号。
    pub order_no: String,
    /// 设备全局编号。
    pub device_id: String,
    /// 物理端口，从 1 开始。
    pub port_no: u8,
}

// ---------------------------------------------------------------------------
// 订单列表 / 详情
// ---------------------------------------------------------------------------

/// 后台订单列表查询条件。
#[derive(Debug, Clone, Default)]
pub struct AdminOrderListQuery {
    /// 页码，从 1 开始（调用方已 sanitize）。
    pub page: u32,
    /// 每页条数，1–100。
    pub page_size: u32,
    /// 按订单号精确匹配。
    pub order_no: Option<String>,
    /// 按设备号精确匹配。
    pub device_id: Option<String>,
    /// 订单内部状态筛选。
    pub status: Option<String>,
    /// 业务状态筛选。
    pub business_status: Option<String>,
    /// 支付状态筛选。
    pub payment_status: Option<String>,
    /// 启动来源筛选。
    pub start_source: Option<String>,
    /// 按站点过滤，None 不限。
    pub station_id: Option<u64>,
    /// 充电时间区间起点（含边界；启动优先、未启动回退创建时间）。
    pub from: Option<DateTime<Utc>>,
    /// 充电时间区间终点（含边界）。
    pub to: Option<DateTime<Utc>>,
}

/// 批量回填站点名称：缺失、已删除站点保留 null。
async fn fill_order_stations(
    pool: &MySqlPool,
    rows: &mut [OrderViewRow],
) -> Result<(), sqlx::Error> {
    let ids: Vec<u64> = rows.iter().filter_map(|r| r.station_id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT `id`, `name` FROM `station` WHERE `deleted_at` IS NULL AND `id` IN ",
    );
    qb.push_tuples(ids, |mut b, id| {
        b.push_bind(id);
    });
    let stations: Vec<(u64, String)> = qb.build_query_as().fetch_all(pool).await?;
    let names: std::collections::HashMap<u64, String> = stations.into_iter().collect();
    for row in rows.iter_mut() {
        if let Some(id) = row.station_id
            && let Some(name) = names.get(&id)
        {
            row.station_name = Some(name.clone());
        }
    }
    Ok(())
}

/// 从冻结快照解析
/// 读取 rule.spec 的方案与计费模式、rule.version 与冻结套餐；
/// 套餐缺失或未通过校验返回 None，不能用当前模板补齐。
pub fn package_from_snapshot(raw: &str) -> Option<Value> {
    #[derive(serde::Deserialize, Default)]
    struct FrozenSpec {
        #[serde(default)]
        scheme: Option<chargepilot_domain::charging::scheme::Scheme>,
        #[serde(default)]
        mode: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct FrozenRule {
        #[serde(default)]
        version: u32,
        #[serde(default)]
        spec: FrozenSpec,
    }
    #[derive(serde::Deserialize)]
    struct FrozenSnapshot {
        #[serde(default)]
        rule: FrozenRule,
        offer: Option<chargepilot_domain::charging::settle::Offer>,
    }
    let frozen: FrozenSnapshot = serde_json::from_str(raw).ok()?;
    let offer = frozen.offer?;
    if !offer.valid() {
        return None;
    }
    let mut pkg = serde_json::Map::new();
    pkg.insert("offer".into(), serde_json::to_value(&offer).ok()?);
    if let Some(scheme) = &frozen.rule.spec.scheme {
        pkg.insert("scheme".into(), serde_json::to_value(scheme).ok()?);
    }
    pkg.insert("billing_mode".into(), Value::String(frozen.rule.spec.mode));
    pkg.insert("rule_version".into(), json!(frozen.rule.version));
    Some(Value::Object(pkg))
}

/// 冻结名称：空白名视为缺失。
fn trimmed_name(value: Option<&Value>) -> Option<String> {
    let name = value?.as_str()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 从已解析的 selected_package 提取冻结方案名称。
pub fn selected_scheme_name(selected: Option<&Value>) -> Option<String> {
    trimmed_name(
        selected
            .and_then(|p| p.get("scheme"))
            .and_then(|s| s.get("name")),
    )
}

/// 从已解析的 selected_package 提取冻结套餐名称。
pub fn selected_package_name(selected: Option<&Value>) -> Option<String> {
    trimmed_name(
        selected
            .and_then(|p| p.get("offer"))
            .and_then(|o| o.get("name")),
    )
}

/// 按订单主键解析冻结套餐。
/// 快照缺失或无效返回 None。
pub async fn order_package_snapshot(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<Value>, sqlx::Error> {
    let raw: Option<String> = query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(raw.as_deref().and_then(package_from_snapshot))
}

/// 读取订单冻结套餐并补充累计刷卡购买。
pub async fn order_selection(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<Value>, sqlx::Error> {
    let Some(mut pkg) = order_package_snapshot(pool, order_id).await? else {
        return Ok(None);
    };
    let card: Option<(i64, u16, u16)> = query_as(
        "SELECT `paid_cents`, `purchased_minutes`, `max_minutes` FROM `card_charge` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?;
    if let Some((paid_cents, purchased_minutes, max_minutes)) = card {
        pkg["card"] = json!({
            "paid_cents": paid_cents,
            "purchased_minutes": purchased_minutes,
            "max_minutes": max_minutes,
        });
    }
    Ok(Some(pkg))
}

/// 批量回填当页订单的冻结方案与套餐名称。
/// 缺失或无效快照保持 null，不读取当前发布规则补造。
async fn fill_scheme_names(pool: &MySqlPool, rows: &mut [OrderViewRow]) -> Result<(), sqlx::Error> {
    let ids: Vec<u64> = rows.iter().map(|r| r.order_id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT `charge_order_id`, CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` IN ",
    );
    qb.push_tuples(ids, |mut b, id| {
        b.push_bind(id as i64);
    });
    let snapshots: Vec<(u64, String)> = qb.build_query_as().fetch_all(pool).await?;
    let names: std::collections::HashMap<u64, (Option<String>, Option<String>)> = snapshots
        .into_iter()
        .map(|(id, raw)| {
            let parsed = package_from_snapshot(&raw);
            (
                id,
                (
                    selected_scheme_name(parsed.as_ref()),
                    selected_package_name(parsed.as_ref()),
                ),
            )
        })
        .collect();
    for row in rows.iter_mut() {
        if let Some((scheme, package)) = names.get(&row.order_id) {
            row.selected_scheme_name = scheme.clone();
            row.selected_package_name = package.clone();
        }
    }
    Ok(())
}

/// 实时计量回填的降级分支：
/// 充电中且已开始的订单输出 live_unavailable 提示，其余行不动。
pub fn apply_live_fallback(rows: &mut [OrderViewRow]) {
    for row in rows.iter_mut() {
        if row.status == ChargeOrderStatus::Charging && row.started_at.is_some() {
            row.live_unavailable = Some("设备计量暂不可读取".into());
        }
    }
}

/// 后台订单分页列表：先计总数，再按创建时间与主键倒序取当页，
/// 最后回填站点名称与冻结套餐名称。
pub async fn list_orders(
    pool: &MySqlPool,
    q: &AdminOrderListQuery,
) -> Result<(Vec<OrderViewRow>, i64), sqlx::Error> {
    let from = q.from;
    let to = q.to;
    let count_sql = format!(
        "SELECT COUNT(*) {ORDER_BASE_FROM} \
         WHERE `c`.`deleted_at` IS NULL \
         AND (? IS NULL OR `c`.`order_no` = ?) \
         AND (? IS NULL OR `c`.`device_id` = ?) \
         AND (? IS NULL OR CAST(`c`.`status` AS CHAR) = ?) \
         AND (? IS NULL OR `c`.`business_status` = ?) \
         AND (? IS NULL OR CAST(`c`.`payment_status` AS CHAR) = ?) \
         AND (? IS NULL OR {START_SOURCE_EXPR} = ?) \
         AND (? IS NULL OR `i`.`station_id` = ?) \
         {ORDER_TIME_GUARD}",
    );
    let total: i64 = query_scalar(&count_sql)
        .bind(&q.order_no)
        .bind(&q.order_no)
        .bind(&q.device_id)
        .bind(&q.device_id)
        .bind(&q.status)
        .bind(&q.status)
        .bind(&q.business_status)
        .bind(&q.business_status)
        .bind(&q.payment_status)
        .bind(&q.payment_status)
        .bind(&q.start_source)
        .bind(&q.start_source)
        .bind(q.station_id)
        .bind(q.station_id)
        .bind(from)
        .bind(from)
        .bind(to)
        .bind(to)
        .bind(from)
        .bind(from)
        .bind(to)
        .bind(to)
        .fetch_one(pool)
        .await?;
    let sql = format!(
        "SELECT {} {ORDER_BASE_FROM} \
         WHERE `c`.`deleted_at` IS NULL \
         AND (? IS NULL OR `c`.`order_no` = ?) \
         AND (? IS NULL OR `c`.`device_id` = ?) \
         AND (? IS NULL OR CAST(`c`.`status` AS CHAR) = ?) \
         AND (? IS NULL OR `c`.`business_status` = ?) \
         AND (? IS NULL OR CAST(`c`.`payment_status` AS CHAR) = ?) \
         AND (? IS NULL OR {START_SOURCE_EXPR} = ?) \
         AND (? IS NULL OR `i`.`station_id` = ?) \
         {ORDER_TIME_GUARD} \
         ORDER BY `c`.`created_at` DESC, `c`.`id` DESC LIMIT ? OFFSET ?",
        order_select(),
    );
    let mut rows: Vec<OrderViewRow> = query_as(&sql)
        .bind(&q.order_no)
        .bind(&q.order_no)
        .bind(&q.device_id)
        .bind(&q.device_id)
        .bind(&q.status)
        .bind(&q.status)
        .bind(&q.business_status)
        .bind(&q.business_status)
        .bind(&q.payment_status)
        .bind(&q.payment_status)
        .bind(&q.start_source)
        .bind(&q.start_source)
        .bind(q.station_id)
        .bind(q.station_id)
        .bind(from)
        .bind(from)
        .bind(to)
        .bind(to)
        .bind(from)
        .bind(from)
        .bind(to)
        .bind(to)
        .bind(q.page_size as i64)
        .bind(((q.page - 1) as i64) * (q.page_size as i64))
        .fetch_all(pool)
        .await?;
    fill_order_stations(pool, &mut rows).await?;
    fill_scheme_names(pool, &mut rows).await?;
    apply_live_fallback(&mut rows);
    Ok((rows, total))
}

/// 按主键取一条订单视图。
pub async fn find_order_view(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<OrderViewRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {} {ORDER_BASE_FROM} WHERE `c`.`deleted_at` IS NULL AND `c`.`id` = ? LIMIT 1",
        order_select(),
    );
    query_as(&sql).bind(order_id).fetch_optional(pool).await
}

/// 读取订单的计费单号。
pub async fn order_calculation_no(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let no: Option<String> = query_scalar(
        "SELECT `calculation_no` FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(no)
}

// ---------------------------------------------------------------------------
// 时间线 / 过程 / 退款
// ---------------------------------------------------------------------------

/// 确认订单存在且未软删除。
pub async fn order_exists(pool: &MySqlPool, order_id: u64) -> Result<bool, sqlx::Error> {
    let id: Option<u64> = query_scalar(
        "SELECT `id` FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(id.is_some())
}

/// 返回订单的事件时间线：按发生时间与 ID 排序，无事件为空数组。
pub async fn list_order_events(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Vec<TimelineRow>, sqlx::Error> {
    let sql = format!(
        "SELECT `event_id`, {} AS `at`, CAST(`event` AS CHAR) AS `event`, `actor`, `detail` \
         FROM `charge_event_log` WHERE `charge_order_id` = ? ORDER BY `occurred_at`, `id`",
        df("`occurred_at`"),
    );
    query_as(&sql).bind(order_id as i64).fetch_all(pool).await
}

/// 读取未删除订单的 ID、单号、设备编号与端口号。
pub async fn find_process_identity(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Option<ProcessIdentity>, sqlx::Error> {
    query_as(
        "SELECT `id`, `order_no`, `device_id`, `port_no` \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map(|row: Option<(u64, String, String, u8)>| {
        row.map(|(id, order_no, device_id, port_no)| ProcessIdentity {
            id,
            order_no,
            device_id,
            port_no,
        })
    })
}

/// 按订单身份分页读取充电过程采样：
/// 按 ID 升序多取一条判断是否有下一页；功率换算为 W、电量换算为 kWh。
pub async fn list_process_points(
    pool: &MySqlPool,
    identity: &ProcessIdentity,
    after_id: u64,
    limit: u32,
) -> Result<(Vec<ProcessPointRow>, Option<u64>), sqlx::Error> {
    let sql = format!(
        "SELECT `id`, {} AS `ts`, \
         CAST(`power_deciwatts` / 10.0 AS DOUBLE) AS `power_w`, \
         CAST(`charged_mwh` / 1000000.0 AS DOUBLE) AS `charged_kwh`, \
         CAST(`remaining_mwh` / 1000000.0 AS DOUBLE) AS `remaining_kwh`, \
         `charged_seconds`, `remaining_seconds`, `signal_strength`, \
         `port_status`, `voltage_v`, `temperature_c`, `device_status` \
         FROM `charge_process` \
         WHERE `order_no` = ? AND `id` > ? AND `charge_order_id` = ? AND `device_id` = ? AND `port_no` = ? \
         ORDER BY `id` ASC LIMIT ?",
        df("`ts`"),
    );
    let mut rows: Vec<ProcessPointRow> = query_as(&sql)
        .bind(&identity.order_no)
        .bind(after_id)
        .bind(identity.id)
        .bind(&identity.device_id)
        .bind(identity.port_no)
        .bind(limit as i64 + 1)
        .fetch_all(pool)
        .await?;
    if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        let next = rows.last().map(|r| r.id);
        Ok((rows, next))
    } else {
        Ok((rows, None))
    }
}

/// 订单关联的退款单（按订单支付单过滤，按 id 倒序；字段取自
/// refund_record 与 refund_review，语义与后台订单详情 API 一致）。
pub async fn list_order_refunds(
    pool: &MySqlPool,
    order_id: u64,
) -> Result<Vec<OrderRefundRow>, sqlx::Error> {
    let sql = format!(
        "SELECT `r`.`id` AS `refund_id`, `r`.`refund_no`, CAST(`r`.`user_id` AS CHAR) AS `user_id`, \
         `r`.`refund_cents`, CAST(`r`.`status` AS CHAR) AS `status`, `r`.`reason`, `r`.`failure_reason`, \
         CAST(`r`.`execution_policy` AS CHAR) AS `execution_policy`, \
         {created} AS `created_at`, {completed} AS `completed_at`, \
         `v`.`first_signer` AS `review_first_signer`, `v`.`first_comment` AS `review_first_comment`, \
         `v`.`second_signer` AS `review_second_signer`, `v`.`second_comment` AS `review_second_comment`, \
         {approved} AS `review_approved_at` \
         FROM `refund_record` `r` \
         LEFT JOIN `refund_review` `v` ON `v`.`refund_record_id` = `r`.`id` \
         WHERE `r`.`deleted_at` IS NULL \
         AND `r`.`payment_order_id` = (SELECT `c`.`payment_order_id` FROM `charge_order` `c` \
              WHERE `c`.`id` = ? AND `c`.`deleted_at` IS NULL) \
         ORDER BY `r`.`id` DESC",
        created = df("`r`.`created_at`"),
        completed = df("`r`.`completed_at`"),
        approved = df("`v`.`approved_at`"),
    );
    query_as(&sql).bind(order_id).fetch_all(pool).await
}

// ---------------------------------------------------------------------------
// 支付单列表
// ---------------------------------------------------------------------------

/// 后台支付单列表查询条件。
#[derive(Debug, Clone, Default)]
pub struct PaymentOrderListQuery {
    /// 页码，从 1 开始。
    pub page: u32,
    /// 每页条数，1–100。
    pub page_size: u32,
    /// 支付单号精确匹配。
    pub order_no: Option<String>,
    /// charge / wallet_recharge。
    pub biz_type: Option<String>,
    /// payment_order.pay_method 的取值。
    pub pay_method: Option<String>,
    /// 金额确认状态筛选（按派生表达式过滤）。
    pub payment_status: Option<String>,
    /// 创建时间起点（含边界）。
    pub from: Option<DateTime<Utc>>,
    /// 创建时间终点（含边界）。
    pub to: Option<DateTime<Utc>>,
}

/// 当前有效关联充电单的标量子查询。
fn linked_charge_order(column: &str) -> String {
    format!(
        "(SELECT `c`.`{column}` FROM `charge_order` `c` WHERE `p`.`biz_type` = 'charge' \
          AND `c`.`payment_order_id` = `p`.`id` AND `c`.`user_id` = `p`.`user_id` \
          AND `c`.`deleted_at` IS NULL ORDER BY `c`.`created_at` DESC, `c`.`id` DESC LIMIT 1)"
    )
}

/// 后台支付单分页列表：创建时间与主键倒序分页，
/// 金额确认状态按派生表达式过滤，充值单始终不关联充电单。
pub async fn list_payment_orders(
    pool: &MySqlPool,
    q: &PaymentOrderListQuery,
) -> Result<(Vec<PaymentOrderRow>, i64), sqlx::Error> {
    let count_sql = format!(
        "SELECT COUNT(*) FROM `payment_order` AS `p` WHERE `p`.`deleted_at` IS NULL \
         AND (? IS NULL OR `p`.`order_no` = ?) \
         AND (? IS NULL OR CAST(`p`.`biz_type` AS CHAR) = ?) \
         AND (? IS NULL OR CAST(`p`.`pay_method` AS CHAR) = ?) \
         AND (? IS NULL OR `p`.`created_at` >= ?) \
         AND (? IS NULL OR `p`.`created_at` <= ?) \
         AND (? IS NULL OR {PAYMENT_STATUS_EXPR} = ?)",
    );
    let total: i64 = query_scalar(&count_sql)
        .bind(&q.order_no)
        .bind(&q.order_no)
        .bind(&q.biz_type)
        .bind(&q.biz_type)
        .bind(&q.pay_method)
        .bind(&q.pay_method)
        .bind(q.from)
        .bind(q.from)
        .bind(q.to)
        .bind(q.to)
        .bind(&q.payment_status)
        .bind(&q.payment_status)
        .fetch_one(pool)
        .await?;
    let sql = format!(
        "SELECT `p`.`id` AS `payment_order_id`, `p`.`order_no`, CAST(`p`.`user_id` AS CHAR) AS `user_id`, \
         CAST(`p`.`biz_type` AS CHAR) AS `biz_type`, CAST(`p`.`pay_method` AS CHAR) AS `pay_method`, \
         CAST(`p`.`status` AS CHAR) AS `status`, {PAYMENT_STATUS_EXPR} AS `payment_status`, \
         `p`.`total_cents`, `p`.`paid_cents`, `p`.`refunded_cents`, \
         {paid_at} AS `paid_at`, {created} AS `created_at`, \
         {linked_id} AS `charge_order_id`, {linked_no} AS `charge_order_no` \
         FROM `payment_order` AS `p` WHERE `p`.`deleted_at` IS NULL \
         AND (? IS NULL OR `p`.`order_no` = ?) \
         AND (? IS NULL OR CAST(`p`.`biz_type` AS CHAR) = ?) \
         AND (? IS NULL OR CAST(`p`.`pay_method` AS CHAR) = ?) \
         AND (? IS NULL OR `p`.`created_at` >= ?) \
         AND (? IS NULL OR `p`.`created_at` <= ?) \
         AND (? IS NULL OR {PAYMENT_STATUS_EXPR} = ?) \
         ORDER BY `p`.`created_at` DESC, `p`.`id` DESC LIMIT ? OFFSET ?",
        paid_at = df("`p`.`paid_at`"),
        created = df("`p`.`created_at`"),
        linked_id = linked_charge_order("id"),
        linked_no = linked_charge_order("order_no"),
    );
    let rows: Vec<PaymentOrderRow> = query_as(&sql)
        .bind(&q.order_no)
        .bind(&q.order_no)
        .bind(&q.biz_type)
        .bind(&q.biz_type)
        .bind(&q.pay_method)
        .bind(&q.pay_method)
        .bind(q.from)
        .bind(q.from)
        .bind(q.to)
        .bind(q.to)
        .bind(&q.payment_status)
        .bind(&q.payment_status)
        .bind(q.page_size as i64)
        .bind(((q.page - 1) as i64) * (q.page_size as i64))
        .fetch_all(pool)
        .await?;
    Ok((rows, total))
}
