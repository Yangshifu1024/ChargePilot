//! 结算域后台门面（票 19 热区批：原 admin_finance.rs 的发票/计量核实/人工退款切片）。

//! 后台财务路由：钱包风控、发票审核、计量核实与人工退款（路径/权限码对齐旧版 finance 装配）。
//! 风控审核要求 finance.wallet_risk.review（客服/财务），解冻要求 finance.wallet_risk.release（仅财务）；
//! 发票/计量/退款的双审与列表权限码沿用旧版（finance.read、invoice.review、billing.meter.review、
//! finance.refund.read、order.refund.review、finance.refund.retry、order.refund.create），
//! 审核类角色与权限的最终判定在服务层事务内复核。

use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/billing/invoices",
            axum::routing::get(list_invoices),
        )
        .route(
            "/api/v1/admin/billing/invoices/{id}/approve",
            axum::routing::post(approve_invoice),
        )
        .route(
            "/api/v1/admin/billing/invoices/{id}/reject",
            axum::routing::post(reject_invoice),
        )
        .route(
            "/api/v1/admin/billing/meter-reviews",
            axum::routing::get(list_meter_reviews),
        )
        .route(
            "/api/v1/admin/billing/meter-reviews/{id}/propose",
            axum::routing::post(propose_meter),
        )
        .route(
            "/api/v1/admin/billing/meter-reviews/{id}/decide",
            axum::routing::post(decide_meter),
        )
        .route(
            "/api/v1/admin/billing/meter-reviews/{id}/resolve-amount",
            axum::routing::post(resolve_meter_amount),
        )
        .route(
            "/api/v1/admin/billing/refunds",
            axum::routing::get(list_refunds),
        )
        .route(
            "/api/v1/admin/billing/refunds/{refund_no}/approve",
            axum::routing::post(approve_refund),
        )
        .route(
            "/api/v1/admin/billing/refunds/{refund_no}/reject",
            axum::routing::post(reject_refund),
        )
        .route(
            "/api/v1/admin/billing/refunds/{refund_no}/retry",
            axum::routing::post(retry_refund),
        )
        .route(
            "/api/v1/admin/orders/{id}/refunds",
            axum::routing::post(manual_refund),
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

// ---------------------------------------------------------------------------
// 发票审核（旧版 invoices.go / invoice_review.go 契约）
// ---------------------------------------------------------------------------

async fn list_invoices(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.read", &rid) {
        return resp;
    }
    match chargepilot_mod_settlement::invoice_admin::list_invoice_requests(&deps.db).await {
        Ok(items) => httpx::ok(json!({ "items": items }), &rid),
        Err(e) => {
            tracing::warn!(error = %e, "发票队列读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                &rid,
            )
        }
    }
}

/// 发票审核输入（旧版 settlement.InvoiceReviewInput；approve/reject 各取所需字段）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvoiceReviewBody {
    #[serde(default)]
    invoice_url: String,
    #[serde(default)]
    reason: String,
}

async fn approve_invoice(
    state: State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    path: Path<u64>,
    body: Bytes,
) -> Response {
    review_invoice(state, guard, headers, path, body, true).await
}

async fn reject_invoice(
    state: State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    path: Path<u64>,
    body: Bytes,
) -> Response {
    review_invoice(state, guard, headers, path, body, false).await
}

/// 发票审核共用处理器（旧版 reviewInvoice：approve 读 invoice_url，reject 读 reason；
/// 审核人须为拥有 invoice.review 的 customer_finance）。
async fn review_invoice(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Bytes,
    approve: bool,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "invoice.review", &rid) {
        return resp;
    }
    if let Some(resp) = require_finance_actor(&guard, &rid) {
        return resp;
    }
    if id == 0 {
        return crate::respond(400, Code::BadRequest as i64, "ID 必须为正整数", &rid);
    }
    let Some(input) = decode_strict::<InvoiceReviewBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    // 审批只认 HTTPS 发票链接，驳回必填原因（旧版 operations.ValidPublicURL / ValidText）。
    let ok_input = if approve {
        valid_public_url(&input.invoice_url)
    } else {
        valid_text(&input.reason, 255)
    };
    if !ok_input {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请填写 HTTPS 发票链接或拒绝原因",
            &rid,
        );
    }
    let actor = guard.profile.clone();
    let ip = real_ip(&headers);
    match chargepilot_mod_settlement::invoice_admin::review_invoice(
        &deps.db,
        &actor,
        id,
        approve,
        &input.invoice_url,
        &input.reason,
        &ip,
        &rid,
    )
    .await
    {
        Ok(status) => httpx::ok(json!({ "review_status": status }), &rid),
        Err(e) => invoice_error(e, &rid),
    }
}

/// 发票审核错误映射（旧版 AdminResourceFailure + asConflict 契约）。
fn invoice_error(
    e: chargepilot_mod_settlement::invoice_admin::InvoiceAdminError,
    rid: &str,
) -> Response {
    use chargepilot_mod_settlement::invoice_admin::InvoiceAdminError as E;
    match &e {
        E::Conflict => crate::respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        E::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        E::Forbidden => crate::respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        E::Storage(m) => {
            tracing::warn!(error = %m, "发票审核处理失败");
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
// 计量核实（旧版 meter_reviews.go 契约）
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MeterPaging {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    keyword: Option<String>,
}

async fn list_meter_reviews(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<MeterPaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.read", &rid) {
        return resp;
    }
    let page = q.page.unwrap_or(1);
    let size = q.page_size.unwrap_or(20);
    if !(1..=1_000_000).contains(&page) || !(1..=100).contains(&size) {
        return crate::respond(400, Code::BadRequest as i64, "分页参数无效", &rid);
    }
    let keyword = q.keyword.unwrap_or_default().trim().to_string();
    if keyword.chars().count() > 128 {
        return crate::respond(400, Code::BadRequest as i64, "关键词过长或状态无效", &rid);
    }
    let status = q.status.unwrap_or_default();
    // 状态白名单（旧版 ReadAdminPage(c, "pending resolved")）。
    if !status.is_empty() && status != "pending" && status != "resolved" {
        return crate::respond(400, Code::BadRequest as i64, "关键词过长或状态无效", &rid);
    }
    match chargepilot_mod_settlement::meter_review::list_manual_fee_reviews(
        &deps.db,
        &status,
        &keyword,
        page as u32,
        size as u32,
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": size}),
            &rid,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "计量核实队列读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                &rid,
            )
        }
    }
}

/// 计量分段输入（旧版 charging.MeterSegment JSON；peak_w 缺省 0 对齐 Go 零值）。
#[derive(Debug, Clone, Deserialize)]
struct SegmentBody {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub energy_wh: u32,
    #[serde(default)]
    pub peak_w: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterProposeBody {
    request_id: String,
    reason: String,
    segments: Vec<SegmentBody>,
}

async fn propose_meter(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Bytes,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "billing.meter.review", &rid) {
        return resp;
    }
    if let Some(resp) = require_finance_actor(&guard, &rid) {
        return resp;
    }
    if id == 0 {
        return crate::respond(400, Code::BadRequest as i64, "ID 必须为正整数", &rid);
    }
    let Some(input) = decode_strict::<MeterProposeBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    let reason = input.reason.trim().to_string();
    // request_id 须为 UUID；依据 1–500 字；分段 1–10080 段（七天按分钟上限）。
    if !valid_uuid(&input.request_id)
        || !valid_text(&reason, 500)
        || input.segments.is_empty()
        || input.segments.len() > 10080
    {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请填写 UUID、核实依据及有效计量分段",
            &rid,
        );
    }
    let segments: Vec<chargepilot_mod_settlement::meter_review::MeterSegmentInput> = input
        .segments
        .iter()
        .map(
            |s| chargepilot_mod_settlement::meter_review::MeterSegmentInput {
                started_at: s.started_at,
                ended_at: s.ended_at,
                energy_wh: s.energy_wh,
                peak_w: s.peak_w,
            },
        )
        .collect();
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::meter_review::propose_meter_review(
        &deps.db,
        Some(&audit_ref),
        guard.profile.id,
        id,
        &input.request_id,
        &reason,
        &segments,
    )
    .await
    {
        Ok(review_id) => {
            // 旧版 propose 返回完整核实行（HTTP 不直接写核实记录）。
            match chargepilot_mod_settlement::meter_review::get_meter_review_row(
                &deps.db, review_id,
            )
            .await
            {
                Ok(Some(row)) => match serde_json::to_value(&row) {
                    Ok(v) => httpx::ok(v, &rid),
                    Err(_) => crate::respond(
                        503,
                        Code::ServiceUnavailable as i64,
                        "数据暂时无法读取或保存，请稍后重试",
                        &rid,
                    ),
                },
                Ok(None) => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", &rid),
                Err(_) => crate::respond(
                    503,
                    Code::ServiceUnavailable as i64,
                    "数据暂时无法读取或保存，请稍后重试",
                    &rid,
                ),
            }
        }
        Err(e) => meter_error(e, &rid),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterDecideBody {
    #[serde(default)]
    review_id: u64,
    approve: Option<bool>,
    #[serde(default)]
    reason: String,
}

async fn decide_meter(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Bytes,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "billing.meter.review", &rid) {
        return resp;
    }
    if let Some(resp) = require_finance_actor(&guard, &rid) {
        return resp;
    }
    if id == 0 {
        return crate::respond(400, Code::BadRequest as i64, "ID 必须为正整数", &rid);
    }
    let Some(input) = decode_strict::<MeterDecideBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    let reason = input.reason.trim().to_string();
    // review_id 必填、approve 必须显式提交（缺失与 false 用 Option 区分）、驳回必填依据。
    let Some(approve) = input.approve else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请提供审核记录、决定和拒绝依据",
            &rid,
        );
    };
    if input.review_id == 0 || (!approve && !valid_text(&reason, 500)) {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请提供审核记录、决定和拒绝依据",
            &rid,
        );
    }
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::meter_review::review_meter(
        &deps.db,
        Some(&audit_ref),
        guard.profile.id,
        id,
        input.review_id,
        approve,
        &reason,
        // 首审人实时权限由模块内事务一致读取（旧版 LookupReviewer 语义）。
        None,
    )
    .await
    {
        Ok(()) => httpx::ok(json!({ "approved": approve }), &rid),
        Err(e) => meter_error(e, &rid),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterResolveBody {
    request_id: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    electric_cents: i64,
    #[serde(default)]
    service_cents: i64,
}

async fn resolve_meter_amount(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Bytes,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "billing.meter.review", &rid) {
        return resp;
    }
    if let Some(resp) = require_finance_actor(&guard, &rid) {
        return resp;
    }
    if id == 0 {
        return crate::respond(400, Code::BadRequest as i64, "ID 必须为正整数", &rid);
    }
    let Some(input) = decode_strict::<MeterResolveBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    let reason = input.reason.trim().to_string();
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::meter_review::resolve_manual_amount(
        &deps.db,
        Some(&audit_ref),
        guard.profile.id,
        id,
        &input.request_id,
        input.electric_cents,
        input.service_cents,
        &reason,
    )
    .await
    {
        Ok(()) => httpx::ok(json!({ "queued": true }), &rid),
        Err(e) => meter_error(e, &rid),
    }
}

/// 计量核实错误映射（旧版 meterFailure 文案契约）。
fn meter_error(e: chargepilot_mod_settlement::meter_review::ReviewError, rid: &str) -> Response {
    use chargepilot_mod_settlement::meter_review::ReviewError as E;
    match &e {
        E::Conflict => crate::respond(
            409,
            2009,
            "计量核实状态已变化、原始数据不一致或审核人重复，请刷新后重试",
            rid,
        ),
        E::InvalidSource => crate::respond(
            400,
            1005,
            "计量分段须连续覆盖原始时段、总电量一致，且各段不得跨不同费率；价格快照必须有效",
            rid,
        ),
        E::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        E::Storage(m) => {
            tracing::warn!(error = %m, "计量核实处理失败");
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
// 人工退款（旧版 refunds.go / refund.go 契约）
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RefundPaging {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
    refund_no: Option<String>,
    keyword: Option<String>,
}

async fn list_refunds(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<RefundPaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.refund.read", &rid) {
        return resp;
    }
    let page = q.page.unwrap_or(1);
    let size = q.page_size.unwrap_or(20);
    if !(1..=1_000_000).contains(&page) || !(1..=100).contains(&size) {
        return crate::respond(400, Code::BadRequest as i64, "分页参数无效", &rid);
    }
    // keyword 不参与退款筛选，但旧版 ReadAdminPage 仍校验其长度。
    if q.keyword
        .map(|k| k.trim().chars().count() > 128)
        .unwrap_or(false)
    {
        return crate::respond(400, Code::BadRequest as i64, "关键词过长或状态无效", &rid);
    }
    let status = q.status.unwrap_or_default();
    // 状态白名单（旧版 money.AllRefundStatuses 全量）。
    if !status.is_empty()
        && !matches!(
            status.as_str(),
            "pending" | "processing" | "success" | "failed" | "rejected"
        )
    {
        return crate::respond(400, Code::BadRequest as i64, "关键词过长或状态无效", &rid);
    }
    let refund_no = q.refund_no.unwrap_or_default();
    if refund_no.chars().count() > 64 {
        return crate::respond(400, Code::BadRequest as i64, "退款单号过长", &rid);
    }
    match chargepilot_mod_settlement::refund_admin::list_refunds(
        &deps.db,
        &status,
        &refund_no,
        page as u32,
        size as u32,
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": size}),
            &rid,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "退款列表读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                &rid,
            )
        }
    }
}

/// 退款审核输入（旧版 settlement.RefundReviewInput；approve 读 approve_comment，reject 读 reason）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundReviewBody {
    #[serde(default)]
    approve_comment: String,
    #[serde(default)]
    reason: String,
}

async fn approve_refund(
    state: State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    path: Path<String>,
    body: Bytes,
) -> Response {
    review_refund(state, guard, headers, path, body, true).await
}

async fn reject_refund(
    state: State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    path: Path<String>,
    body: Bytes,
) -> Response {
    review_refund(state, guard, headers, path, body, false).await
}

/// 退款双审共用处理器（旧版 reviewRefund：审核人须为拥有 order.refund.review 的
/// customer_finance；第一次通过返回 awaiting_second，第二人通过返回 approved）。
async fn review_refund(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(refund_no): Path<String>,
    body: Bytes,
    approve: bool,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.refund.review", &rid) {
        return resp;
    }
    if let Some(resp) = require_finance_actor(&guard, &rid) {
        return resp;
    }
    let Some(input) = decode_strict::<RefundReviewBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    let ok_input = if approve {
        valid_text(&input.approve_comment, 255)
    } else {
        valid_text(&input.reason, 255)
    };
    if !ok_input {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请填写 1–255 字审核意见",
            &rid,
        );
    }
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::refund_admin::review_manual_refund(
        &deps.db,
        Some(&audit_ref),
        guard.profile.id,
        &refund_no,
        approve,
        &input.approve_comment,
        &input.reason,
        // 首审人实时权限由模块内事务一致读取（旧版 LookupSigner 语义）。
        None,
    )
    .await
    {
        Ok(outcome) => {
            let status = match outcome {
                chargepilot_mod_settlement::refund_admin::ReviewOutcome::AwaitingSecond => {
                    "awaiting_second"
                }
                chargepilot_mod_settlement::refund_admin::ReviewOutcome::Approved => "approved",
                chargepilot_mod_settlement::refund_admin::ReviewOutcome::Rejected => "rejected",
            };
            httpx::ok(json!({ "review_status": status }), &rid)
        }
        Err(e) => refund_error(e, &rid),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundRetryBody {
    #[serde(default)]
    reason: String,
}

async fn retry_refund(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(refund_no): Path<String>,
    body: Bytes,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.refund.retry", &rid) {
        return resp;
    }
    let Some(input) = decode_strict::<RefundRetryBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    if !valid_text(&input.reason, 255) {
        return crate::respond(400, Code::BadRequest as i64, "请填写重试原因", &rid);
    }
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::refund_admin::retry_refund(
        &deps.db,
        Some(&audit_ref),
        &refund_no,
        &input.reason,
    )
    .await
    {
        Ok(_) => httpx::ok(json!({ "scheduled": true }), &rid),
        Err(e) => refund_error(e, &rid),
    }
}

/// 人工退款申请输入（旧版 settlement.ManualRefundRequest，同时作为首次审计快照）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualRefundBody {
    request_id: String,
    amount_cents: i64,
    reason: String,
}

async fn manual_refund(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Bytes,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "order.refund.create", &rid) {
        return resp;
    }
    if id == 0 {
        return crate::respond(400, Code::BadRequest as i64, "ID 必须为正整数", &rid);
    }
    let Some(input) = decode_strict::<ManualRefundBody>(&body) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请求内容无效或包含未知字段",
            &rid,
        );
    };
    if !valid_uuid(&input.request_id) || input.amount_cents <= 0 || !valid_text(&input.reason, 255)
    {
        return crate::respond(
            400,
            1005,
            "请填写有效请求编号、正整数退款分值和退款原因",
            &rid,
        );
    }
    let ip = real_ip(&headers);
    let audit_ref = chargepilot_mod_settlement::AuditView {
        actor: &guard.profile,
        client_ip: &ip,
        request_id: &rid,
    };
    match chargepilot_mod_settlement::refund_admin::request_manual_refund(
        &deps.db,
        Some(&audit_ref),
        guard.profile.id,
        id,
        &input.request_id,
        input.amount_cents,
        &input.reason,
    )
    .await
    {
        Ok((created, refund_no)) => httpx::ok(
            json!({
                "created": created,
                "refund_no": refund_no,
                "request_id": input.request_id,
            }),
            &rid,
        ),
        Err(chargepilot_mod_settlement::refund_admin::RefundAdminError::NotSettled) => {
            crate::respond(
                409,
                Code::StateConflict as i64,
                "订单尚未完成费用结算，请结算后再申请退款",
                &rid,
            )
        }
        Err(e) => refund_error(e, &rid),
    }
}

/// 退款错误映射（旧版 asConflict → ErrConflict 的 409 文案契约）。
fn refund_error(
    e: chargepilot_mod_settlement::refund_admin::RefundAdminError,
    rid: &str,
) -> Response {
    use chargepilot_mod_settlement::refund_admin::RefundAdminError as E;
    match &e {
        E::Conflict => crate::respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        E::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        E::NotSettled => crate::respond(
            409,
            Code::StateConflict as i64,
            "订单尚未完成费用结算，请结算后再申请退款",
            rid,
        ),
        E::Storage(m) => {
            tracing::warn!(error = %m, "人工退款处理失败");
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
// 共用助手（校验与审计上下文，语义对齐旧版 operations/admin 助手）
// ---------------------------------------------------------------------------

/// 审核类端点的财务角色约束（旧版 RequireFinanceActor：403 文案逐字对齐）。
fn require_finance_actor(guard: &AdminGuard, rid: &str) -> Option<Response> {
    if guard.profile.role == "customer_finance" {
        None
    } else {
        Some(crate::respond(
            403,
            1003,
            "须由拥有审核权限的客户财务人员执行",
            rid,
        ))
    }
}

/// 文本校验（旧版 operations.ValidText：非空白且不超过 max 个字符）。
fn valid_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max
}

/// HTTPS 公开链接校验（旧版 operations.ValidPublicURL：≤512、scheme https、
/// 有主机名且不含 user:pass 凭据段；用 http::Uri 解析避免新增依赖）。
fn valid_public_url(s: &str) -> bool {
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    match s.parse::<axum::http::Uri>() {
        Ok(uri) => {
            uri.scheme_str() == Some("https")
                && !uri.host().unwrap_or_default().is_empty()
                && !uri
                    .authority()
                    .map(|a| a.as_str().contains('@'))
                    .unwrap_or(true)
        }
        Err(_) => false,
    }
}

/// UUID 请求号校验（旧版 RequestIDPattern：8-4-4-4-12 十六进制）。
fn valid_uuid(v: &str) -> bool {
    let parts: Vec<&str> = v.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, l)| p.len() == l && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn real_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// 严格 JSON 解码（旧版 DecodeResource：≤64KiB、拒绝未知字段、单一 JSON 值）。
fn decode_strict<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    if body.len() > 64 * 1024 {
        return None;
    }
    let mut it = serde_json::Deserializer::from_slice(body).into_iter::<T>();
    match (it.next(), it.next()) {
        (Some(Ok(v)), None) => Some(v),
        _ => None,
    }
}
