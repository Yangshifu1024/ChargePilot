//! 充电域用户端门面（票 18 冷区批：原 user_charging.rs 归位）。

use crate::state::{ApiDeps, ChargingStopState, UserSessionBase};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_domain::order::ChargeOrderStatus;
use chargepilot_gateway_runtime::snapshot::DeviceSnapshot;
use chargepilot_mod_order::user_query::{self, OrderRecord};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/user/charge/ongoing",
            axum::routing::get(charge_ongoing),
        )
        .route(
            "/api/v1/user/charge/ongoing/snapshot",
            axum::routing::get(charge_snapshot),
        )
        .route(
            "/api/v1/user/charge/ongoing/curve",
            axum::routing::get(charge_curve),
        )
        .route(
            "/api/v1/user/charge/history",
            axum::routing::get(charge_history),
        )
        .route(
            "/api/v1/user/charge/{order_no}/curve",
            axum::routing::get(charge_history_curve),
        )
        .route(
            "/api/v1/user/charge/{order_no}",
            axum::routing::get(charge_detail),
        )
}

/// 停止路由（`charging_stop` 装配开关 → ChargingStopState 必达，票 17）。
pub fn stop_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/user/charging/{order_no}/stop",
            axum::routing::post(user_stop),
        )
        // 旧版请求体形状（{order_no}），小程序 charging.controller.js 调用。
        .route(
            "/api/v1/user/charge/stop",
            axum::routing::post(user_stop_body),
        )
}

// ---------------------------------------------------------------------------
// 查询错误映射（旧版 queryFailure）
// ---------------------------------------------------------------------------

/// 查询失败分类（对齐旧版 queryFailure 的状态码与文案）。
enum QueryFail {
    /// 输入校验（400 1005）。
    Input(&'static str),
    /// 窗口或时间冲突（409 2009）。
    Conflict(&'static str),
    /// 超过保留期（422 2018）。
    Expired(&'static str),
    /// 订单缺失或越权（404 1004，与缺失共用同一文案，不泄漏他人订单）。
    NotFound,
    /// 读取失败（503 5003）。
    Storage(sqlx::Error),
}

impl From<sqlx::Error> for QueryFail {
    fn from(e: sqlx::Error) -> Self {
        QueryFail::Storage(e)
    }
}

fn query_failure(f: QueryFail, rid: &str) -> Response {
    match f {
        QueryFail::Input(m) => crate::respond(400, Code::BadRequest as i64, m, rid),
        QueryFail::Conflict(m) => crate::respond(409, Code::Conflict as i64, m, rid),
        QueryFail::Expired(m) => crate::respond(422, Code::QueryExpired as i64, m, rid),
        QueryFail::NotFound => crate::respond(404, Code::NotFound as i64, "充电订单不存在", rid),
        QueryFail::Storage(e) => {
            tracing::warn!(error = %e, "用户订单查询失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "订单暂时无法读取",
                rid,
            )
        }
    }
}

/// 订单号参数解析（旧版 queryOrderNo：order_id 优先，其次 order_no）。
/// 收两个字段而非查询结构体，供 OrderNoQuery 与 WindowQuery 两类端点共用。
fn order_param(order_id: &Option<String>, order_no: &Option<String>) -> String {
    let from = order_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| order_no.as_deref().filter(|s| !s.trim().is_empty()));
    from.unwrap_or_default().trim().to_string()
}

/// 归属解析：输入非法 400，缺失或越权 404（同一文案）。
async fn resolve_or_fail(deps: &ApiDeps, uid: u64, raw: &str) -> Result<OrderRecord, QueryFail> {
    if raw.is_empty() || raw.len() > 64 {
        return Err(QueryFail::Input("订单号无效"));
    }
    user_query::resolve_owned(&deps.db, uid, raw)
        .await?
        .ok_or(QueryFail::NotFound)
}

/// datetime(3) 文本（DATE_FORMAT 输出的 RFC3339 形态）解析为 UTC 时刻。
fn parse_ts(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

// ---------------------------------------------------------------------------
// 六个查询端点
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct OrderNoQuery {
    order_id: Option<String>,
    order_no: Option<String>,
}

#[derive(Deserialize)]
struct WindowQuery {
    order_id: Option<String>,
    order_no: Option<String>,
    window: Option<String>,
}

#[derive(Deserialize)]
struct GranularityQuery {
    granularity: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
}

/// 充电中订单（旧版 ongoing：无记录输出空状态，不报错）。
async fn charge_ongoing(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    match user_query::ongoing_order(&deps.db, uid).await {
        Ok(Some(r)) => httpx::ok(
            json!({
                "order_no": r.order_no,
                "status": r.status,
                "device_id": r.device_id,
                "port_no": r.port_no,
                "started_at": r.started_at,
                "ended_at": r.ended_at,
            }),
            &rid,
        ),
        Ok(None) => httpx::ok(json!({"order_no": "", "status": ""}), &rid),
        Err(e) => query_failure(e.into(), &rid),
    }
}

/// 历史订单分页（旧版 history：状态词表校验 + 费用批量回填）。
async fn charge_history(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    // 旧版 ReadPaging：page 默认 1 上限 100000，page_size 默认 20 上限 100。
    let page = q.page.unwrap_or(1);
    let size = q.page_size.unwrap_or(20);
    if !(1..=100_000).contains(&page) {
        return crate::respond(400, Code::BadRequest as i64, "分页参数无效", &rid);
    }
    if !(1..=100).contains(&size) {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "分页参数无效：page_size 为 1–100",
            &rid,
        );
    }
    const STATUSES: [&str; 8] = [
        "pending_payment",
        "paid",
        "charging",
        "completed",
        "cancelled",
        "failed",
        "refunding",
        "refunded",
    ];
    let status = q.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = status
        && !STATUSES.contains(&s)
    {
        return crate::respond(400, Code::BadRequest as i64, "订单状态无效", &rid);
    }
    match user_query::list_history(&deps.db, uid, status, page, size).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": size}),
            &rid,
        ),
        Err(e) => query_failure(e.into(), &rid),
    }
}

/// 订单详情（旧版 Detail：base + 支付摘要 + 退款列表）。
async fn charge_detail(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Path(order_no): Path<String>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let row = match resolve_or_fail(&deps, uid, &order_no).await {
        Ok(r) => r,
        Err(f) => return query_failure(f, &rid),
    };
    let out = match async {
        let mut out = user_query::base_view(&deps.db, &row).await?;
        // 支付摘要缺失按无支付处理（旧版 Funds：缺失不是数据库故障，字段省略）。
        if let Some(pid) = row.payment_order_id
            && let Some(v) = user_query::payment_brief(&deps.db, pid).await?
        {
            out["payment"] = v;
        }
        out["refunds"] = json!(user_query::order_refunds(&deps.db, row.id).await?);
        Ok::<_, sqlx::Error>(out)
    }
    .await
    {
        Ok(v) => v,
        Err(e) => return query_failure(e.into(), &rid),
    };
    httpx::ok(out, &rid)
}

/// 实时快照（旧版 Snapshot：轮询提示 + 两分钟窗口观测 + 估费提示）。
async fn charge_snapshot(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<OrderNoQuery>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let row = match resolve_or_fail(&deps, uid, &order_param(&q.order_id, &q.order_no)).await {
        Ok(r) => r,
        Err(f) => return query_failure(f, &rid),
    };
    let out = match build_snapshot(&deps, &row).await {
        Ok(v) => v,
        Err(e) => return query_failure(e.into(), &rid),
    };
    httpx::ok(out, &rid)
}

/// 组装快照视图（旧版 UserService.Snapshot）。
async fn build_snapshot(deps: &ApiDeps, row: &OrderRecord) -> Result<Value, sqlx::Error> {
    let mut out = user_query::base_view(&deps.db, row).await?;
    let poll = row.status.is_ongoing();
    out["poll_continue"] = json!(poll);
    out["next_poll_after_ms"] = json!(5000);
    let mut available = false;
    out["telemetry_available"] = json!(available);
    if row.status != ChargeOrderStatus::Charging {
        return Ok(out);
    }
    let Some(started) = row.started_at.as_deref().and_then(parse_ts) else {
        return Ok(out);
    };
    let now = chrono::Utc::now();
    out["elapsed_seconds"] = json!((now - started).num_seconds().max(0));
    // 估费（旧版 LiveService.Fill）未在新仓库接线：沿旧版依赖缺失分支输出提示，
    // 不用当前模板或部分读数冒充估算费用。
    out["live_unavailable"] = json!("设备计量暂不可读取");
    let from = (now - chrono::Duration::minutes(2)).max(started);
    if from >= now {
        return Ok(out);
    }
    // 两分钟窗口观测：原始遥测优先，表空时回退 SnapshotStore（含 db 回退）。
    if let Some((ts, power, voltage, temperature, meter)) =
        user_query::latest_observation(&deps.db, &row.device_id, row.port_no, from, now).await?
    {
        out["telemetry_at"] = json!(ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        if power.is_some() || meter.is_some() {
            available = true;
        }
        if let Some(v) = power {
            out["current_power_w"] = json!(v);
        }
        if let Some(v) = voltage {
            out["voltage_v"] = json!(v);
        }
        if let Some(v) = temperature {
            out["temperature_c"] = json!(v);
        }
        if let Some(v) = meter {
            out["charged_kwh"] = json!(v);
        }
    } else if let Some(store) = &deps.snapshots {
        let db = deps.db.clone();
        if let Some(snap) = store
            .get_or_load(&row.device_id, |device_id| async move {
                let row: Option<(Option<chrono::DateTime<chrono::Utc>>,)> = sqlx::query_as(
                    "SELECT `last_seen_at` FROM `device` WHERE `device_id` = ? LIMIT 1",
                )
                .bind(&device_id)
                .fetch_optional(&db)
                .await
                .ok()?;
                row.map(|(seen,)| DeviceSnapshot {
                    device_id,
                    online: seen.is_some(),
                    last_heartbeat_at: seen,
                    ..DeviceSnapshot::default()
                })
            })
            .await
            && let Some(p) = snap.port(row.port_no)
            && (p.power_deci_watts > 0 || p.charged_mwh > 0 || p.charged_seconds > 0)
        {
            available = true;
            out["telemetry_at"] = json!(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            if p.power_deci_watts > 0 {
                out["current_power_w"] = json!(f64::from(p.power_deci_watts) / 10.0);
            }
            if p.charged_mwh > 0 {
                out["charged_kwh"] = json!(f64::from(p.charged_mwh) / 1_000_000.0);
            }
            if snap.voltage_v > 0 {
                out["voltage_v"] = json!(f64::from(snap.voltage_v));
            }
        }
    }
    out["telemetry_available"] = json!(available);
    Ok(out)
}

/// 实时曲线窗口（旧版 Curve：last_30min/last_2h/last_24h）。
async fn charge_curve(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<WindowQuery>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let row = match resolve_or_fail(&deps, uid, &order_param(&q.order_id, &q.order_no)).await {
        Ok(r) => r,
        Err(f) => return query_failure(f, &rid),
    };
    // 窗口校验（旧版 windowDuration：空取首窗口，未知值报输入错误）。
    let window = q.window.as_deref().map(str::trim).unwrap_or("");
    let duration = match window {
        "" => chrono::Duration::minutes(30),
        "last_30min" => chrono::Duration::minutes(30),
        "last_2h" => chrono::Duration::hours(2),
        "last_24h" => chrono::Duration::hours(24),
        _ => {
            return query_failure(
                QueryFail::Input("window 须为允许的 last_30min、last_2h 或 last_24h"),
                &rid,
            );
        }
    };
    let now = chrono::Utc::now();
    let from = now - duration;
    let series = match user_query::window_points(
        &deps.db,
        &row.device_id,
        row.port_no,
        from,
        now,
        user_query::MAX_QUERY_POINTS,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return query_failure(e.into(), &rid),
    };
    let count = series.len();
    httpx::ok(
        json!({
            "order_no": row.order_no,
            "window": if window.is_empty() { "last_30min" } else { window },
            "from": from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "to": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "series": series,
            "count": count,
        }),
        &rid,
    )
}

/// 历史曲线（旧版 HistoricalCurve：聚合粒度与点数校验 + 摘要）。
async fn charge_history_curve(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Path(order_no): Path<String>,
    Query(q): Query<GranularityQuery>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let row = match resolve_or_fail(&deps, uid, &order_no).await {
        Ok(r) => r,
        Err(f) => return query_failure(f, &rid),
    };
    let Some(started) = row.started_at.as_deref().and_then(parse_ts) else {
        return query_failure(QueryFail::Conflict("订单尚未开始充电"), &rid);
    };
    let granularity = q.granularity.as_deref().unwrap_or("15min");
    if granularity != "15min" && granularity != "hourly" {
        return query_failure(QueryFail::Input("granularity 须为 15min 或 hourly"), &rid);
    }
    let now = chrono::Utc::now();
    let ended = row.ended_at.as_deref().and_then(parse_ts).unwrap_or(now);
    if started >= ended {
        return query_failure(QueryFail::Conflict("订单时间窗无效"), &rid);
    }
    if started < now - chrono::Duration::hours(24 * 365 * 3) {
        return query_failure(QueryFail::Expired("历史遥测已超过三年保留期"), &rid);
    }
    let bucket_secs: i64 = if granularity == "hourly" { 3600 } else { 900 };
    let trunc = |t: chrono::DateTime<chrono::Utc>| {
        chrono::DateTime::from_timestamp(t.timestamp() - t.timestamp().rem_euclid(bucket_secs), 0)
            .unwrap_or(t)
    };
    let points = (trunc(ended).timestamp() - trunc(started).timestamp()) / bucket_secs + 1;
    if points > user_query::MAX_QUERY_POINTS {
        return query_failure(
            QueryFail::Input("订单时间窗超过当前粒度的最大点数，请使用更粗粒度"),
            &rid,
        );
    }
    let (series, mut summary, boundary) = match user_query::historical_curve(
        &deps.db,
        &row.device_id,
        row.port_no,
        started,
        ended,
        granularity,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return query_failure(e.into(), &rid),
    };
    // 摘要电量优先使用订单 DECIMAL 文本（不从实时观测重算）。
    if let Some(kwh) = &row.charged_kwh {
        summary["total_kwh"] = json!(kwh);
    }
    httpx::ok(
        json!({
            "order_id": row.id,
            "order_no": row.order_no,
            "granularity": granularity,
            "series": series,
            "summary": summary,
            "boundary_approximate": boundary,
        }),
        &rid,
    )
}

// ---------------------------------------------------------------------------
// 用户停止（双形状共用停止链）
// ---------------------------------------------------------------------------

/// 新版路径形状：POST /charging/{order_no}/stop。
async fn user_stop(
    State(st): State<ChargingStopState>,
    headers: HeaderMap,
    Path(order_no): Path<String>,
) -> Response {
    run_stop(&st, &headers, &order_no).await
}

/// 旧版请求体形状：POST /charge/stop，体 {order_no}（必填、最长 64）。
async fn user_stop_body(
    State(st): State<ChargingStopState>,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let order_no = body
        .map(|b| b.0)
        .and_then(|v| {
            v.get("order_no")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if order_no.is_empty() || order_no.len() > 64 {
        return crate::respond(400, Code::BadRequest as i64, "invalid order number", &rid);
    }
    run_stop(&st, &headers, &order_no).await
}

/// 停止链：归属校验 + 计费截止冻结 + 停止命令下发。
/// 域服务经 ChargingStopState 必达（票 17：路由注册绑定装配开关）。
async fn run_stop(st: &ChargingStopState, headers: &HeaderMap, order_no: &str) -> Response {
    let rid = httpx::request_id_from(headers);
    let stop = &st.stop;
    let uid = match crate::user_guard::user_id_from(&st.base, headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match stop.stop(order_no, uid).await {
        Ok(active) => httpx::ok(
            json!({
                "charge_order_id": active.charge_order_id,
                "order_no": active.order_no,
                "status": "stopping",
            }),
            &rid,
        ),
        // 错误映射经 errormap::stop_error 收编（票 17：三枢纽之一）。
        Err(e) => crate::errormap::stop_error(e, &rid),
    }
}
