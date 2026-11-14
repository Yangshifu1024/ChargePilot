//! C 端钱包路由：余额、流水、充值记录、退款进度与退款申请（路径对齐旧版）。
//! 充值申请入口（POST /wallet/recharge）依赖支付渠道准备，随批 3 接线。

use crate::deps::{ApiDeps, UserSessionBase, WalletState};
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

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

#[derive(Deserialize)]
struct Paging {
    page: Option<i64>,
    page_size: Option<i64>,
    direction: Option<String>,
}

fn paging(q: &Paging) -> (u32, u32) {
    (
        q.page.unwrap_or(1).max(1) as u32,
        q.page_size.unwrap_or(20).clamp(1, 100) as u32,
    )
}

/// 钱包域错误 → HTTP（码值与文案沿用旧版字面量契约）。
pub fn wallet_error(e: chargepilot_mod_wallet::WalletError, rid: &str) -> Response {
    use chargepilot_mod_wallet::WalletError as E;
    match &e {
        E::Forbidden => crate::respond(403, 1003, "没有操作权限", rid),
        E::Frozen => crate::respond(409, 2009, "钱包已冻结，请等待审核", rid),
        E::InsufficientBalance => crate::respond(409, 2009, "可用余额不足", rid),
        E::Identity => crate::respond(409, 2009, "请求号冲突或钱包已冻结", rid),
        E::RequestConflict => crate::respond(409, 2009, "申请冲突或原充值可退额度不足", rid),
        E::PrepayNotConfigured | E::PrepayUnavailable => {
            crate::respond(502, 3001, "支付渠道暂不可用，申请已保留", rid)
        }
        E::NotFound => crate::respond(404, 1004, "记录不存在", rid),
        E::Inactive | E::InvalidBalance | E::Storage(_) => {
            tracing::warn!(error = %e, "钱包请求处理失败");
            let api = chargepilot_platform::error::ApiError::from(anyhow::anyhow!(e.to_string()));
            httpx::error(&api, rid)
        }
    }
}

async fn wallet_balance(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match chargepilot_mod_wallet::query::balance(&deps.db, uid).await {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(e) => wallet_error(e, &rid),
    }
}

async fn wallet_txns(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<Paging>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let (page, size) = paging(&q);
    let direction = q.direction.as_deref().filter(|d| !d.is_empty());
    match chargepilot_mod_wallet::query::transactions(&deps.db, uid, direction, page, size).await {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => wallet_error(e, &rid),
    }
}

async fn wallet_recharges(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<Paging>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let (page, size) = paging(&q);
    match chargepilot_mod_wallet::query::recharges(&deps.db, uid, page, size, chrono::Utc::now())
        .await
    {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => wallet_error(e, &rid),
    }
}

async fn wallet_refunds(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    Query(q): Query<Paging>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let (page, size) = paging(&q);
    match chargepilot_mod_wallet::query::refunds(&deps.db, uid, page, size).await {
        Ok(p) => httpx::ok(
            json!({"items": p.items, "total": p.total, "page": p.page, "page_size": p.page_size}),
            &rid,
        ),
        Err(e) => wallet_error(e, &rid),
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
    AxumJson(body): AxumJson<RefundBody>,
) -> Response {
    let rid = rid_of(&headers);
    let uid = match crate::user_guard::user_id_from(&st.base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let wallet = &st.wallet;
    let input = chargepilot_mod_wallet::refund::RefundInput {
        request_id: body.request_id,
        amount_cents: body.amount_cents,
        reason: body.reason,
    };
    match wallet.refund.request(uid, &input).await {
        Ok(receipt) => httpx::ok(serde_json::to_value(receipt).unwrap_or(json!(null)), &rid),
        Err(e) => wallet_error(e, &rid),
    }
}
