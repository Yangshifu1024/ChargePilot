//! 钱包域后台路由：风控事件列表、审核与解冻。
//! 权限码 `finance.wallet_risk.review`（客服/财务可审）、`finance.wallet_risk.release`（仅财务可解冻）。

use crate::errormap::{rid_of, risk_error};
use crate::guard::AdminGuard;
use crate::guard::deny;
use crate::params::AdminPaging;
use crate::state::ApiDeps;
use axum::Json as AxumJson;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

// ---- 风控----

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
    if let Some(resp) = deny(
        &guard,
        chargepilot_mod_ops::perms::FINANCE_WALLET_RISK_REVIEW,
        &rid,
    ) {
        return resp;
    }
    let Some(wallet) = &deps.wallet else {
        return crate::respond(503, Code::ServiceUnavailable as i64, "钱包服务未装配", &rid);
    };
    // AdminGuard 构造时已加载实时权限快照，列表过滤与服务层判定一致。
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
    if let Some(resp) = deny(
        &guard,
        chargepilot_mod_ops::perms::FINANCE_WALLET_RISK_REVIEW,
        &rid,
    ) {
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
    if let Some(resp) = deny(
        &guard,
        chargepilot_mod_ops::perms::FINANCE_WALLET_RISK_RELEASE,
        &rid,
    ) {
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
