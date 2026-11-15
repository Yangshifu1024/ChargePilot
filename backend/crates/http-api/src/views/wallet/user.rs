//! 钱包域用户端门面（票 18 冷区批：原 user_wallet.rs 归位）。

use crate::errormap::rid_of;
use crate::params::UserPaging;
use crate::state::{ApiDeps, WalletState};
use crate::user_guard::UserGuard;
use axum::Json as AxumJson;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 查询路由（无条件注册：只消费库连接）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/user/wallet/balance",
            axum::routing::get(wallet_balance),
        )
        .route("/api/v1/user/wallet/txns", axum::routing::get(wallet_txns))
        .route(
            "/api/v1/user/wallet/recharges",
            axum::routing::get(wallet_recharges),
        )
        .route(
            "/api/v1/user/wallet/refunds",
            axum::routing::get(wallet_refunds),
        )
}

/// 退款申请路由（`wallet` 装配开关 → WalletState 必达，票 17）。
pub fn refund_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        // 旧版契约：申请入口为单数 /refund，查询为复数 /refunds。
        .route(
            "/api/v1/user/wallet/refund",
            axum::routing::post(wallet_refund),
        )
}

#[derive(Deserialize)]
struct Paging {
    direction: Option<String>,
}

async fn wallet_balance(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_wallet::query::balance(&deps.db, uid).await {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(e) => crate::errormap::wallet_error(e, &rid),
    }
}

async fn wallet_txns(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
    Query(q): Query<Paging>,
) -> Response {
    let rid = rid_of(&headers);
    let direction = q.direction.as_deref().filter(|d| !d.is_empty());
    match chargepilot_mod_wallet::query::transactions(&deps.db, uid, direction, page, page_size)
        .await
    {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => crate::errormap::wallet_error(e, &rid),
    }
}

async fn wallet_recharges(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_wallet::query::recharges(
        &deps.db,
        uid,
        page,
        page_size,
        chrono::Utc::now(),
    )
    .await
    {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => crate::errormap::wallet_error(e, &rid),
    }
}

async fn wallet_refunds(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    UserPaging { page, page_size }: UserPaging,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_wallet::query::refunds(&deps.db, uid, page, page_size).await {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => crate::errormap::wallet_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct RefundBody {
    request_id: String,
    amount_cents: i64,
    #[serde(default)]
    reason: String,
}

/// 余额退款申请：重放同请求号返回首次回执；3 次/5 分钟触发频次冻结转人工。
async fn wallet_refund(
    State(st): State<WalletState>,
    headers: HeaderMap,
    UserGuard(uid): UserGuard,
    AxumJson(body): AxumJson<RefundBody>,
) -> Response {
    let rid = rid_of(&headers);
    let wallet = &st.wallet;
    let input = chargepilot_mod_wallet::refund::RefundInput {
        request_id: body.request_id,
        amount_cents: body.amount_cents,
        reason: body.reason,
    };
    match wallet.refund.request(uid, &input).await {
        Ok(receipt) => httpx::ok(serde_json::to_value(receipt).unwrap_or(json!(null)), &rid),
        Err(e) => crate::errormap::wallet_error(e, &rid),
    }
}
