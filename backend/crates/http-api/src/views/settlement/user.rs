//! 结算域用户端门面（票 19 热区批：原 user_settlement.rs 归位；admin 切片随本票并入）。

//! C 端账单与发票路由：账单列表/详情/已读、开票申请与我的发票（路径与响应
//! 形状对齐旧版 internal/http/user/settlement/{bill,invoice}.go）。
//!
//! 挂载行：.merge(user_settlement::router())
//! （lib.rs build_router 中与其余 user 路由并列；模块声明 pub mod user_settlement;）

use crate::params::UserPaging;
use crate::state::ApiDeps;
use crate::user_guard::UserGuard;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_settlement::UserInvoiceError;
use chargepilot_mod_settlement::user_invoice::{self, InvoiceInput};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/user/bills", axum::routing::get(bill_list))
        .route("/api/v1/user/bills/{id}", axum::routing::get(bill_detail))
        .route(
            "/api/v1/user/bills/{id}/read",
            axum::routing::post(bill_mark_read),
        )
        .route(
            "/api/v1/user/invoice/apply",
            axum::routing::post(invoice_apply),
        )
        .route("/api/v1/user/invoice/my", axum::routing::get(invoice_my))
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// 路径账单 ID 解析：非法或为零一律 400（旧版 strconv.ParseUint + id==0 校验）。
fn bill_id_of(raw: &str, rid: &str) -> Result<u64, Response> {
    match raw.parse::<u64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(crate::respond(
            400,
            Code::BadRequest as i64,
            "账单编号无效",
            rid,
        )),
    }
}

/// GET /api/v1/user/bills：分页返回当前用户的账单。
async fn bill_list(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_settlement::bill::list_user_bills(&deps.db, uid, page, page_size).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": page_size}),
            &rid,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "账单列表读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "账单暂时无法读取",
                &rid,
            )
        }
    }
}

/// GET /api/v1/user/bills/:id：按账单 ID 与当前用户查询详情；越权与不存在统一 404。
async fn bill_detail(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    let bill_id = match bill_id_of(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match chargepilot_mod_settlement::bill::load_user_bill(&deps.db, uid, bill_id).await {
        Ok(Some(bill)) => httpx::ok(bill, &rid),
        Ok(None) => crate::respond(404, Code::NotFound as i64, "账单不存在", &rid),
        Err(e) => {
            tracing::warn!(error = %e, "账单详情读取失败");
            crate::respond(404, Code::NotFound as i64, "账单不存在", &rid)
        }
    }
}

/// POST /api/v1/user/bills/:id/read：幂等已读标记；查不到 404，保存失败 503。
async fn bill_mark_read(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    let bill_id = match bill_id_of(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match chargepilot_mod_settlement::bill::mark_bill_read(&deps.db, uid, bill_id).await {
        Ok(true) => httpx::ok(json!({"id": bill_id.to_string(), "read": true}), &rid),
        Ok(false) => crate::respond(404, Code::NotFound as i64, "账单不存在", &rid),
        Err(e) => {
            tracing::warn!(error = %e, "账单已读保存失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "账单状态暂时无法保存",
                &rid,
            )
        }
    }
}

#[derive(Deserialize)]
struct InvoiceApplyBody {
    request_id: String,
    order_no: String,
    #[serde(default)]
    invoice_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tax_no: String,
    #[serde(default)]
    email: String,
}

/// POST /api/v1/user/invoice/apply：校验开票参数并提交申请；金额由订单回执确定。
/// 错误映射对齐旧版 applyInvoice 的字面量契约（状态码、业务码与文案）。
async fn invoice_apply(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    body: Option<axum::Json<InvoiceApplyBody>>,
) -> Response {
    let rid = rid_of(&headers);
    let Some(axum::Json(body)) = body else {
        return crate::respond(400, Code::BadRequest as i64, "发票申请无效", &rid);
    };
    // 前置校验顺序与文案对齐旧版 HTTP 层（服务层仍全量校验兜底）。
    if body.request_id.trim().is_empty() || body.order_no.trim().is_empty() {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请提供 UUID 请求号与订单号",
            &rid,
        );
    }
    if !matches!(body.invoice_type.as_str(), "normal" | "vat_special") {
        return crate::respond(400, Code::BadRequest as i64, "发票类型无效", &rid);
    }
    let title = body.title.trim();
    if title.is_empty() || title.chars().count() > 128 {
        return crate::respond(400, Code::BadRequest as i64, "发票抬头无效", &rid);
    }
    if body.tax_no.trim().chars().count() > 32 {
        return crate::respond(400, Code::BadRequest as i64, "税号过长", &rid);
    }
    if body.email.len() > 128 || (!body.email.is_empty() && !body.email.contains('@')) {
        return crate::respond(400, Code::BadRequest as i64, "邮箱格式无效", &rid);
    }
    let input = InvoiceInput {
        request_id: body.request_id,
        order_no: body.order_no,
        invoice_type: body.invoice_type,
        title: body.title,
        tax_no: body.tax_no,
        email: body.email,
    };
    match user_invoice::apply_invoice(&deps.db, uid, &input).await {
        Ok(out) => httpx::ok(
            json!({
                "invoice_no": out.invoice_no,
                "id": out.id,
                "review_status": out.review_status,
                "total_cents": out.total_cents,
            }),
            &rid,
        ),
        Err(e) => invoice_error(&e, &rid),
    }
}

/// 发票域错误 → HTTP（旧版 applyInvoice 的 switch 字面量契约）。
fn invoice_error(e: &UserInvoiceError, rid: &str) -> Response {
    match e {
        UserInvoiceError::Unbilled => crate::respond(
            409,
            Code::Conflict as i64,
            "订单尚未计费，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::OrderNotFound => {
            crate::respond(404, Code::NotFound as i64, "订单不存在", rid)
        }
        UserInvoiceError::OrderLookup(_) => crate::respond(
            503,
            Code::ServiceUnavailable as i64,
            "订单暂时无法读取",
            rid,
        ),
        UserInvoiceError::Unfinished => crate::respond(
            409,
            Code::Conflict as i64,
            "订单尚未完成，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::ZeroFee => crate::respond(
            409,
            Code::Conflict as i64,
            "订单金额为零，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::Duplicate => {
            crate::respond(409, Code::Conflict as i64, "该订单已申请过发票", rid)
        }
        UserInvoiceError::InvalidInput => {
            crate::respond(400, Code::BadRequest as i64, "发票申请无效", rid)
        }
        UserInvoiceError::Storage(m) => {
            tracing::warn!(error = %m, "发票申请保存失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法保存，请稍后重试",
                rid,
            )
        }
    }
}

/// GET /api/v1/user/invoice/my：分页返回当前用户的发票申请。
async fn invoice_my(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = rid_of(&headers);
    match user_invoice::list_user_invoices(&deps.db, uid, page, page_size).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page, "page_size": page_size}),
            &rid,
        ),
        Err(e) => {
            tracing::warn!(error = %e, "发票记录读取失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "发票记录暂时无法读取",
                &rid,
            )
        }
    }
}
