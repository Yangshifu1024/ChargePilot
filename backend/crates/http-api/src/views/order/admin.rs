//! 订单域后台门面（票 18：先归位反馈工单区块；admin_orders 热区批并入）。
use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::params::AdminPaging;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
struct CasePaging {
    status: Option<String>,
}

/// 反馈状态白名单（旧版 ReadAdminPage(c, "pending processed closed")）。
async fn list_feedback(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    AdminPaging { page, page_size }: AdminPaging,
    Query(q): Query<CasePaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "feedback.read", &rid) {
        return resp;
    }
    // 状态过滤白名单语义收编共享提取器（票 17）；错误文案逐字保留。
    if crate::params::StatusFilter::allowed(
        q.status.as_deref(),
        &["pending", "processed", "closed"],
        &rid,
        "关键词过长或状态无效",
    )
    .is_err()
    {
        return crate::respond(400, Code::BadRequest as i64, "关键词过长或状态无效", &rid);
    }
    match chargepilot_mod_order::feedback::list(
        &deps.db,
        q.status.as_deref().unwrap_or(""),
        page,
        page_size,
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": page_size}),
            &rid,
        ),
        Err(e) => feedback_error(e, &rid),
    }
}

async fn reply_feedback(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "feedback.reply", &rid) {
        return resp;
    }
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let action = v["action"].as_str().unwrap_or_default().to_string();
    let reply_content = v["reply_content"].as_str().unwrap_or_default().to_string();
    match chargepilot_mod_order::feedback::reply(
        &deps.db,
        &guard.profile,
        id,
        &action,
        &reply_content,
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok(()) => httpx::ok(json!({"saved": true}), &rid),
        Err(e) => feedback_error(e, &rid),
    }
}

/// 反馈错误映射（旧版 AdminResourceFailure + FeedbackInputError）。
fn feedback_error(e: chargepilot_mod_order::FeedbackError, rid: &str) -> Response {
    use chargepilot_mod_order::FeedbackError as E;
    match &e {
        E::Input(msg) => crate::respond(400, Code::BadRequest as i64, msg, rid),
        E::Conflict => crate::respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        E::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        E::Forbidden => crate::respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        E::Storage(_) => {
            tracing::warn!(cause = %e, "反馈请求处理失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// 审计日志
// ---------------------------------------------------------------------------

/// 反馈工单路由（票 18：原 admin_misc 区块归位）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/feedback", axum::routing::get(list_feedback))
        .route(
            "/api/v1/admin/feedback/{id}/reply",
            axum::routing::post(reply_feedback),
        )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

fn deny(guard: &AdminGuard, permission: &str, rid: &str) -> Option<Response> {
    if guard.has(permission) {
        None
    } else {
        Some(ops_error(OpsError::Permission, rid))
    }
}

fn real_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// 反馈工单
// ---------------------------------------------------------------------------

// ---- 订单管理（票 19：原 admin_orders.rs 整体并入）----

// 订单域后台门面（票 19 热区批：原 admin_orders.rs 整体并入；feedback 见本目录 admin.rs）。

use chargepilot_mod_order::admin_query::{self, AdminOrderListQuery, PaymentOrderListQuery};
use chrono::Datelike;

/// 订单内部状态全量词表（旧版 orderdomain.AllStatuses）。
const ORDER_STATUSES: [&str; 8] = [
    "pending_payment",
    "paid",
    "charging",
    "completed",
    "cancelled",
    "failed",
    "refunding",
    "refunded",
];

/// 业务状态词表（旧版 AllBusinessStatuses）。
const BUSINESS_STATUSES: [&str; 3] = ["pending_start", "charging", "completed"];

/// 订单支付状态词表（旧版 AllPaymentStatuses）。
const PAYMENT_STATUSES: [&str; 4] = ["pending", "paid", "refunded", "partial_refunded"];

/// 启动来源词表（旧版 AllStartSources）。
const START_SOURCES: [&str; 3] = ["payment", "balance", "card"];

/// 支付单用途词表（旧版 money.AllPaymentBizTypes）。
const PAYMENT_BIZ_TYPES: [&str; 2] = ["charge", "wallet_recharge"];

/// 支付单入口开放的支付方式（旧版 AdminOneOf 白名单 "wechat balance"）。
const PAY_METHODS: [&str; 2] = ["wechat", "balance"];

pub fn admin_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/orders", axum::routing::get(list_orders))
        .route(
            "/api/v1/admin/orders/{id}",
            axum::routing::get(order_detail),
        )
        .route(
            "/api/v1/admin/orders/{id}/timeline",
            axum::routing::get(order_timeline),
        )
        .route(
            "/api/v1/admin/orders/{id}/process",
            axum::routing::get(order_process),
        )
        .route(
            "/api/v1/admin/orders/{id}/refunds",
            axum::routing::get(order_refunds),
        )
        .route(
            "/api/v1/admin/payment-orders",
            axum::routing::get(payment_orders),
        )
}

/// 存储层错误统一映射（旧版 AdminResourceFailure 的 503 分支文案）。
fn storage_error(e: sqlx::Error, rid: &str) -> Response {
    tracing::warn!(cause = %e, "后台订单查询失败");
    ops_error(OpsError::Storage(anyhow::anyhow!(e.to_string())), rid)
}

/// 路径参数 :id 解析（旧版 AdminPathID）：非负整数或零均拒绝。
fn parse_path_id(raw: &str, rid: &str) -> Result<u64, Response> {
    match raw.parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(crate::respond(
            400,
            Code::BadRequest as i64,
            "ID 必须为正整数",
            rid,
        )),
    }
}

/// 关键词与状态校验（旧版 ReadAdminPage：关键词 ≤128 字，状态落白名单）。
fn validate_keyword_status(
    keyword: Option<&str>,
    status: Option<&str>,
    allowed: &[&str],
    rid: &str,
) -> Result<(), Response> {
    if keyword.is_some_and(|k| k.chars().count() > 128)
        || status.is_some_and(|s| !s.is_empty() && !allowed.contains(&s))
    {
        return Err(crate::respond(
            400,
            Code::BadRequest as i64,
            "关键词过长或状态无效",
            rid,
        ));
    }
    Ok(())
}

/// RFC3339 时间参数解析（旧版 time.Parse(RFC3339Nano)）。
fn parse_time(
    raw: Option<&str>,
    message: &str,
    rid: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, Response> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(t) => Ok(Some(t.with_timezone(&chrono::Utc))),
        Err(_) => Err(crate::respond(400, Code::BadRequest as i64, message, rid)),
    }
}

fn one_of(value: Option<&str>, allowed: &[&str]) -> bool {
    match value {
        None => true,
        Some(v) => v.is_empty() || allowed.contains(&v),
    }
}

// ---------------------------------------------------------------------------
// 订单列表
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OrderListParams {
    keyword: Option<String>,
    status: Option<String>,
    order_no: Option<String>,
    device_id: Option<String>,
    business_status: Option<String>,
    payment_status: Option<String>,
    start_source: Option<String>,
    station_id: Option<String>,
    started_from: Option<String>,
    started_to: Option<String>,
}

async fn list_orders(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    AdminPaging { page, page_size }: AdminPaging,
    Query(q): Query<OrderListParams>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    if let Err(resp) = validate_keyword_status(
        q.keyword.as_deref(),
        q.status.as_deref(),
        &ORDER_STATUSES,
        &rid,
    ) {
        return resp;
    }
    if !one_of(q.business_status.as_deref(), &BUSINESS_STATUSES)
        || !one_of(q.payment_status.as_deref(), &PAYMENT_STATUSES)
        || !one_of(q.start_source.as_deref(), &START_SOURCES)
    {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "业务状态、支付状态或启动来源无效",
            &rid,
        );
    }
    if q.order_no.as_deref().is_some_and(|v| v.len() > 64)
        || q.device_id.as_deref().is_some_and(|v| v.len() > 64)
    {
        return crate::respond(400, Code::BadRequest as i64, "订单号或设备号过长", &rid);
    }
    let station_id = match q.station_id.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw.parse::<u64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return crate::respond(400, Code::BadRequest as i64, "站点 ID 无效", &rid),
        },
    };
    let from = match parse_time(q.started_from.as_deref(), "时间格式须为 ISO 8601", &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let to = match parse_time(q.started_to.as_deref(), "时间格式须为 ISO 8601", &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "开始时间不能晚于结束时间",
            &rid,
        );
    }
    let query = AdminOrderListQuery {
        page,
        page_size,
        order_no: q.order_no.filter(|v| !v.is_empty()),
        device_id: q.device_id.filter(|v| !v.is_empty()),
        status: q.status.filter(|v| !v.is_empty()),
        business_status: q.business_status.filter(|v| !v.is_empty()),
        payment_status: q.payment_status.filter(|v| !v.is_empty()),
        start_source: q.start_source.filter(|v| !v.is_empty()),
        station_id,
        from,
        to,
    };
    match admin_query::list_orders(&deps.db, &query).await {
        Ok((items, total)) => httpx::ok(
            json!({
                "items": items,
                "total": total,
                "page": page,
                "page_size": page_size,
            }),
            &rid,
        ),
        Err(e) => storage_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 订单详情
// ---------------------------------------------------------------------------

async fn order_detail(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    let order_id = match parse_path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut row = match admin_query::find_order_view(&deps.db, order_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return ops_error(OpsError::not_found("记录不存在或已删除"), &rid);
        }
        Err(e) => return storage_error(e, &rid),
    };
    // 冻结套餐、名称与站点名称（缺失快照或无效套餐保持 null，不读当前价格）。
    match admin_query::order_selection(&deps.db, order_id).await {
        Ok(selected) => {
            row.selected_scheme_name = admin_query::selected_scheme_name(selected.as_ref());
            row.selected_package_name = admin_query::selected_package_name(selected.as_ref());
            row.selected_package = selected;
        }
        Err(e) => return storage_error(e, &rid),
    }
    match admin_query::order_calculation_no(&deps.db, order_id).await {
        Ok(calculation_no) => {
            row.billing = Some(json!({"calculation_no": calculation_no}));
        }
        Err(e) => return storage_error(e, &rid),
    }
    // 仅对实时持有退款创建权限的操作人回填申请人，供界面显示退款入口（旧版语义）。
    if guard.has("order.refund.create") {
        row.refund_applicant_id = Some(guard.profile.id.to_string());
    }
    let rows = [row];
    let mut rows = rows;
    admin_query::apply_live_fallback(&mut rows);
    httpx::ok(
        serde_json::to_value(&rows[0]).unwrap_or(serde_json::Value::Null),
        &rid,
    )
}

// ---------------------------------------------------------------------------
// 事件时间线
// ---------------------------------------------------------------------------

async fn order_timeline(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    let order_id = match parse_path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // 先确认订单存在，避免对缺失订单返回一份空时间线（旧版语义）。
    match admin_query::order_exists(&deps.db, order_id).await {
        Ok(true) => {}
        Ok(false) => {
            return ops_error(OpsError::not_found("记录不存在或已删除"), &rid);
        }
        Err(e) => return storage_error(e, &rid),
    }
    match admin_query::list_order_events(&deps.db, order_id).await {
        Ok(timeline) => httpx::ok(json!({"order_id": order_id, "timeline": timeline}), &rid),
        Err(e) => storage_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 充电过程采样（游标分页）
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProcessParams {
    after_id: Option<String>,
    limit: Option<String>,
}

/// 游标与页长解析（旧版 parseOrderProcessQuery + ProcessService.Read 校验）。
fn parse_process_params(q: &ProcessParams, rid: &str) -> Result<(u64, u32), Response> {
    // 游标分页校验收编共享提取器（票 17）；错误文案逐字保留旧契约。
    let parsed = crate::params::CursorParams::parse(
        q.after_id.as_deref(),
        q.limit.as_deref(),
        rid,
        (
            "过程游标或分页参数无效",
            "过程游标必须非负，limit 为 1–2000",
        ),
    )?;
    Ok((parsed.after_id, parsed.limit))
}

async fn order_process(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ProcessParams>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    let order_id = match parse_path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let (after_id, limit) = match parse_process_params(&q, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let identity = match admin_query::find_process_identity(&deps.db, order_id).await {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            return ops_error(OpsError::not_found("记录不存在或已删除"), &rid);
        }
        Err(e) => return storage_error(e, &rid),
    };
    // 读取失败统一为过程暂不可用（旧版 ErrProcessUnavailable 语义），不泄露底层错误。
    match admin_query::list_process_points(&deps.db, &identity, after_id, limit).await {
        Ok((items, next)) => httpx::ok(json!({"items": items, "next_after_id": next}), &rid),
        Err(e) => {
            tracing::warn!(cause = %e, "充电过程读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "充电过程暂时无法读取，请稍后重试",
                &rid,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// 订单关联退款单
// ---------------------------------------------------------------------------

async fn order_refunds(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    let order_id = match parse_path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match admin_query::order_exists(&deps.db, order_id).await {
        Ok(true) => {}
        Ok(false) => {
            return ops_error(OpsError::not_found("记录不存在或已删除"), &rid);
        }
        Err(e) => return storage_error(e, &rid),
    }
    match admin_query::list_order_refunds(&deps.db, order_id).await {
        Ok(items) => httpx::ok(
            json!({"order_id": order_id, "items": items, "total": items.len()}),
            &rid,
        ),
        Err(e) => storage_error(e, &rid),
    }
}

// ---------------------------------------------------------------------------
// 支付单列表
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PaymentOrderParams {
    keyword: Option<String>,
    status: Option<String>,
    order_no: Option<String>,
    biz_type: Option<String>,
    pay_method: Option<String>,
    payment_status: Option<String>,
    created_from: Option<String>,
    created_to: Option<String>,
}

async fn payment_orders(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    AdminPaging { page, page_size }: AdminPaging,
    Query(q): Query<PaymentOrderParams>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.read", &rid) {
        return resp;
    }
    // 旧版：状态白名单为空（任何非空 status 拒绝），关键词 ≤128 字。
    if let Err(resp) = validate_keyword_status(q.keyword.as_deref(), q.status.as_deref(), &[], &rid)
    {
        return resp;
    }
    if q.order_no
        .as_deref()
        .is_some_and(|v| v.chars().count() > 64)
        || !one_of(q.biz_type.as_deref(), &PAYMENT_BIZ_TYPES)
        || !one_of(q.pay_method.as_deref(), &PAY_METHODS)
        || !one_of(q.payment_status.as_deref(), &PAYMENT_STATUSES)
    {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "支付单号、用途、支付方式或支付状态无效",
            &rid,
        );
    }
    let from = match parse_time(
        q.created_from.as_deref(),
        "时间格式须为 ISO 8601，且在有效日期范围内",
        &rid,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let to = match parse_time(
        q.created_to.as_deref(),
        "时间格式须为 ISO 8601，且在有效日期范围内",
        &rid,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // 旧版 paymentTimeRange：MySQL 可表示的年份范围（1000–9999）。
    for t in [from, to].into_iter().flatten() {
        let year = t.year();
        if !(1000..=9999).contains(&year) {
            return crate::respond(
                400,
                Code::BadRequest as i64,
                "时间格式须为 ISO 8601，且在有效日期范围内",
                &rid,
            );
        }
    }
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "开始时间不能晚于结束时间",
            &rid,
        );
    }
    let query = PaymentOrderListQuery {
        page,
        page_size,
        order_no: q.order_no.filter(|v| !v.is_empty()),
        biz_type: q.biz_type.filter(|v| !v.is_empty()),
        pay_method: q.pay_method.filter(|v| !v.is_empty()),
        payment_status: q.payment_status.filter(|v| !v.is_empty()),
        from,
        to,
    };
    match admin_query::list_payment_orders(&deps.db, &query).await {
        Ok((items, total)) => httpx::ok(
            json!({
                "items": items,
                "total": total,
                "page": page,
                "page_size": page_size,
            }),
            &rid,
        ),
        Err(e) => storage_error(e, &rid),
    }
}
