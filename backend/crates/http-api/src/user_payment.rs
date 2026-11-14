//! C 端支付路由：扫码套餐、下单支付准备、模拟确认与钱包充值申请。
//! 路径对齐旧版：POST /scan/offers、/scan/start、/development/payments/confirm、
//! /wallet/recharge；模拟确认仅在 PAYMENT_MODE=simulation 时装配。

use crate::deps::{ApiDeps, PaymentState, UserSessionBase};
use axum::Json as AxumJson;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// 下单与充值路由（`payment` 装配开关 → PaymentState 必达，票 17）。
pub fn checkout_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/user/scan/offers",
            axum::routing::post(payment_offers),
        )
        .route(
            "/api/v1/user/scan/start",
            axum::routing::post(payment_start),
        )
        .route(
            "/api/v1/user/wallet/recharge",
            axum::routing::post(wallet_recharge),
        )
}

/// 模拟确认路由（`simulation_payment` 布尔开关，404 语义保留）。
pub fn simulation_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/user/development/payments/confirm",
        axum::routing::post(payment_simulation_confirm),
    )
}

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

#[derive(Deserialize)]
struct OffersBody {
    port_code: String,
}

/// 端口当前套餐与展示配置；无需登录（旧版 offers）。
async fn payment_offers(
    State(st): State<PaymentState>,
    headers: HeaderMap,
    AxumJson(body): AxumJson<OffersBody>,
) -> Response {
    let rid = rid_of(&headers);
    let checkout = &st.checkout;
    if body.port_code.is_empty() {
        return crate::respond(400, 1005, "请求参数无效", &rid);
    }
    match checkout.offers(&body.port_code).await {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(e) => checkout_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct StartBody {
    client_request_id: String,
    port_code: String,
    offer_id: u64,
    /// 券发放 ID（可选，十进制字符串或数字）。当前套餐下单不支持券（旧版
    /// 互斥约束），非 0 由服务层返回冲突；字段保留以对齐旧版请求契约。
    #[serde(default)]
    coupon_grant_id: Option<String>,
}

/// 下单与支付准备（旧版 start）：冻结意图与快照，返回支付参数。
async fn payment_start(
    State(st): State<PaymentState>,
    headers: HeaderMap,
    AxumJson(body): AxumJson<StartBody>,
) -> Response {
    let rid = rid_of(&headers);
    let checkout = &st.checkout;
    let uid = match crate::user_guard::user_id_from(&st.base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let input = chargepilot_mod_payment::checkout::CheckoutInput {
        client_request_id: body.client_request_id,
        port_code: body.port_code,
        offer_id: body.offer_id,
        // 旧版 DTO 为十进制字符串（避免 JS 精度丢失），兼容数字。
        coupon_grant_id: body
            .coupon_grant_id
            .as_ref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
    };
    match checkout.start(uid, &input).await {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(e) => checkout_error(e, &rid),
    }
}

#[derive(Deserialize)]
struct ConfirmBody {
    merchant_order_no: String,
}

/// 模拟确认当前用户的支付（旧版 DevelopmentAPI.confirm；走支付成功事务）。
async fn payment_simulation_confirm(
    State(deps): State<Arc<ApiDeps>>,
    State(base): State<UserSessionBase>,
    headers: HeaderMap,
    AxumJson(body): AxumJson<ConfirmBody>,
) -> Response {
    let rid = rid_of(&headers);
    if !deps.simulation_payment {
        return crate::respond(404, 1004, "未开放模拟支付", &rid);
    }
    let uid = match crate::user_guard::user_id_from(&base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if body.merchant_order_no.is_empty() || body.merchant_order_no.len() > 64 {
        return crate::respond(400, 1005, "请求参数无效", &rid);
    }
    match chargepilot_mod_payment::simulation::confirm_user(&deps.db, uid, &body.merchant_order_no)
        .await
    {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(chargepilot_mod_payment::simulation::SimulationError::NotFound) => {
            crate::respond(404, 1004, "支付单不存在", &rid)
        }
        Err(chargepilot_mod_payment::simulation::SimulationError::NotSimulation) => {
            crate::respond(409, 3001, "支付不是模拟渠道", &rid)
        }
        Err(chargepilot_mod_payment::simulation::SimulationError::Conflict) => {
            crate::respond(409, 3001, "模拟支付冲突", &rid)
        }
        Err(chargepilot_mod_payment::simulation::SimulationError::Storage(m)) => {
            tracing::warn!(error = %m, "模拟确认处理失败");
            crate::respond(503, 5001, "模拟确认暂不可用", &rid)
        }
    }
}

#[derive(Deserialize)]
struct RechargeBody {
    request_id: String,
    amount_cents: i64,
}

/// 钱包充值申请（旧版 RechargeService.Create）：幂等建单 + 预支付参数。
async fn wallet_recharge(
    State(st): State<PaymentState>,
    headers: HeaderMap,
    AxumJson(body): AxumJson<RechargeBody>,
) -> Response {
    let rid = rid_of(&headers);
    let recharge = &st.wallet_recharge;
    let uid = match crate::user_guard::user_id_from(&st.base, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let input = chargepilot_mod_wallet::recharge::RechargeInput {
        request_id: body.request_id,
        amount_cents: body.amount_cents,
    };
    match recharge.create(uid, &input).await {
        Ok(v) => httpx::ok(serde_json::to_value(v).unwrap_or(json!(null)), &rid),
        Err(e) => crate::user_wallet::wallet_error(e, &rid),
    }
}

/// 下单错误映射（对齐旧版 CheckoutError Kind → 状态码/文案）。
fn checkout_error(e: chargepilot_mod_payment::CheckoutError, rid: &str) -> Response {
    use chargepilot_mod_payment::CheckoutError as E;
    match &e {
        E::InvalidInput => crate::respond(400, 1005, "请求参数无效", rid),
        E::NotFound => crate::respond(404, 1004, "设备或端口不存在", rid),
        E::Paused => crate::respond(409, 2009, "设备已停用", rid),
        E::PortBusy => crate::respond(409, 2009, "端口已被占用", rid),
        E::OfferUnavailable => crate::respond(409, 2009, "套餐不可售", rid),
        E::Pricing => crate::respond(409, 2009, "计费方案不可用", rid),
        E::Conflict => crate::respond(409, 2009, "支付申请冲突", rid),
        E::Provider => crate::respond(502, 3001, "支付渠道不可用", rid),
        E::Storage(m) => {
            tracing::warn!(error = %m, "下单处理失败");
            crate::respond(503, 5003, "下单暂不可用，请稍后重试", rid)
        }
    }
}
