//! 后台财务路由：钱包风控、发票审核、计量核实与人工退款（路径/权限码对齐旧版 finance 装配）。
//! 风控审核要求 finance.wallet_risk.review（客服/财务），解冻要求 finance.wallet_risk.release（仅财务）；
//! 发票/计量/退款的双审与列表权限码沿用旧版（finance.read、invoice.review、billing.meter.review、
//! finance.refund.read、order.refund.review、finance.refund.retry、order.refund.create），
//! 审核类角色与权限的最终判定在服务层事务内复核。

use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::params::AdminPaging;
use crate::state::ApiDeps;
use axum::Json as AxumJson;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

// ---- 风控（票 19：原 admin_finance.rs 的 wallet-risks 切片）----

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/billing/wallet-risks",
            axum::routing::get(list_wallet_risks),
        )
        .route(
            "/api/v1/admin/billing/wallet-risks/{request_id}/review",
            axum::routing::post(review_wallet_risk),
        )
        .route(
            "/api/v1/admin/billing/wallet-risks/{request_id}/release",
            axum::routing::post(release_wallet_risk),
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

#[derive(Deserialize)]
struct RiskPaging {
    status: Option<String>,
}

async fn list_wallet_risks(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    AdminPaging { page, page_size }: AdminPaging,
    Query(q): Query<RiskPaging>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.wallet_risk.review", &rid) {
        return resp;
    }
    let Some(wallet) = &deps.wallet else {
        return crate::respond(503, Code::ServiceUnavailable as i64, "钱包服务未装配", &rid);
    };
    // guard 内已有实时权限快照（AdminGuard 构造时加载），与旧版 actor 语义一致。
    match wallet
        .risk
        .list(
            &guard.profile,
            q.status.as_deref().unwrap_or(""),
            page,
            page_size,
        )
        .await
    {
        Ok((items, total)) => httpx::ok(json!({"items": items, "total": total}), &rid),
        Err(e) => risk_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct ReviewBody {
    approved: Option<bool>,
    comment: String,
}

async fn review_wallet_risk(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    AxumJson(body): AxumJson<ReviewBody>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.wallet_risk.review", &rid) {
        return resp;
    }
    let Some(wallet) = &deps.wallet else {
        return crate::respond(503, Code::ServiceUnavailable as i64, "钱包服务未装配", &rid);
    };
    let input = chargepilot_mod_wallet::risk::RiskReviewInput {
        approved: body.approved,
        comment: body.comment,
    };
    match wallet
        .risk
        .review(&guard.profile, &request_id, &input, &rid, &rid)
        .await
    {
        Ok(response) => httpx::ok(response, &rid),
        Err(e) => risk_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct ReleaseBody {
    comment: String,
}

async fn release_wallet_risk(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    AxumJson(body): AxumJson<ReleaseBody>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "finance.wallet_risk.release", &rid) {
        return resp;
    }
    let Some(wallet) = &deps.wallet else {
        return crate::respond(503, Code::ServiceUnavailable as i64, "钱包服务未装配", &rid);
    };
    let input = chargepilot_mod_wallet::risk::RiskReleaseInput {
        comment: body.comment,
    };
    match wallet
        .risk
        .release(&guard.profile, &request_id, &input, &rid, &rid)
        .await
    {
        Ok(response) => httpx::ok(response, &rid),
        Err(e) => risk_error(e, &rid),
    }
}

fn risk_error(e: chargepilot_mod_wallet::WalletError, rid: &str) -> Response {
    use chargepilot_mod_wallet::WalletError as E;
    match &e {
        E::Forbidden => ops_error(OpsError::Permission, rid),
        E::RequestConflict | E::NotFound => {
            ops_error(OpsError::bad_input("业务状态冲突".to_string()), rid)
        }
        E::Frozen | E::InsufficientBalance => {
            ops_error(OpsError::bad_input("业务状态冲突".to_string()), rid)
        }
        E::Identity
        | E::PrepayNotConfigured
        | E::PrepayUnavailable
        | E::Inactive
        | E::InvalidBalance
        | E::Storage(_) => {
            tracing::warn!(error = %e, "风控请求处理失败");
            let api = chargepilot_platform::error::ApiError::from(anyhow::anyhow!(e.to_string()));
            httpx::error(&api, rid)
        }
    }
}
