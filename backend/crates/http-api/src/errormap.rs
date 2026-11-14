//! 统一错误映射（票 17）：各域错误 → HTTP 信封的唯一归宿。
//!
//! 收编三处既有映射枢纽——身份错误直映（原 lib.rs）、运营错误映射函数
//! （原 admin_auth.rs）、停止错误直映（原 user_charging.rs）——码值与文案
//! 沿用旧版字面量契约；直发码的码表缺口（4291/3001/2018/5000）补进
//! `platform::error::Code`，一码多 HTTP 状态经 [`respond`] 显式状态参数支持。

use axum::response::{IntoResponse, Response};
use chargepilot_mod_identity::IdentityError as IdE;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::error::{ApiError, Code};
use chargepilot_platform::httpx;

/// 从请求头解析 Bearer 凭证（<scheme> <token>）。
pub fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// 直发指定状态/码/文案的信封（一码多状态：调用方显式给 HTTP 状态）。
pub fn respond(status: u16, code: i64, message: &str, request_id: &str) -> Response {
    let body = chargepilot_platform::httpx::Envelope {
        code,
        message: message.to_string(),
        data: serde_json::Value::Null,
        request_id: request_id.to_string(),
        trace_id: request_id.to_string(),
        errors: Vec::new(),
    };
    let mut resp = (
        axum::http::StatusCode::from_u16(status)
            .unwrap_or_else(|_| panic!("契约内状态码非法: {status}")),
        axum::Json(body),
    )
        .into_response();
    resp.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

/// identity 域错误 → HTTP 响应（码值与文案沿用旧版字面量契约）。
pub fn identity_error(err: IdE, request_id: &str) -> Response {
    match &err {
        IdE::InvalidRefresh => respond(
            401,
            Code::Unauthorized as i64,
            "refresh token invalid",
            request_id,
        ),
        IdE::TokenInvalid => respond(
            401,
            Code::Unauthorized as i64,
            "access token invalid",
            request_id,
        ),
        IdE::SessionExpired => respond(
            401,
            Code::Unauthorized as i64,
            "session expired",
            request_id,
        ),
        IdE::AccountFrozen => respond(403, Code::Forbidden as i64, "account frozen", request_id),
        IdE::ExchangeFailed(_) => respond(
            502,
            Code::ChannelInteraction as i64,
            "WeChat login failed",
            request_id,
        ),
        IdE::Storage(_) => {
            let api = ApiError::from(anyhow::anyhow!(err.to_string()));
            chargepilot_platform::httpx::error(&api, request_id)
        }
    }
}

/// OpsError → HTTP 响应（运营域统一映射；4291 登录频次带 Retry-After）。
pub fn ops_error(err: OpsError, request_id: &str) -> Response {
    match &err {
        OpsError::Credentials => respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            request_id,
        ),
        OpsError::Locked(msg) => respond(423, Code::AccountLocked as i64, msg, request_id),
        OpsError::UsedTotp => respond(
            401,
            1001,
            "该验证码已被使用，请等待下一个验证码",
            request_id,
        ),
        OpsError::InvalidTotp => respond(
            401,
            Code::Unauthorized as i64,
            "验证码无效或已过期",
            request_id,
        ),
        OpsError::InvalidChallenge => respond(
            401,
            Code::Unauthorized as i64,
            "登录已失效，请重新登录",
            request_id,
        ),
        OpsError::LoginRate => {
            let mut resp = respond(
                429,
                Code::LoginRateLimited as i64,
                "登录尝试过于频繁，请稍后再试",
                request_id,
            );
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("60"));
            resp
        }
        OpsError::Permission => respond(403, Code::Forbidden as i64, "没有操作权限", request_id),
        OpsError::Input(msg) => respond(400, Code::BadRequest as i64, msg, request_id),
        OpsError::Conflict(msg) => respond(409, Code::Conflict as i64, msg, request_id),
        OpsError::NotFound(msg) => respond(404, Code::NotFound as i64, msg, request_id),
        OpsError::Storage(_) => {
            tracing::warn!(cause = %err, "后台登录存储失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                request_id,
            )
        }
    }
}

/// StopError → HTTP 响应（充电停止链直映；原 user_charging.rs 的 match 收编）。
pub fn stop_error(err: chargepilot_mod_charging::StopError, request_id: &str) -> Response {
    match err {
        chargepilot_mod_charging::StopError::NotFound => respond(
            404,
            Code::NotFound as i64,
            "订单不存在或不可停止",
            request_id,
        ),
        chargepilot_mod_charging::StopError::PortIdentity => {
            respond(409, Code::Conflict as i64, "端口身份缺失", request_id)
        }
        chargepilot_mod_charging::StopError::SendFailed(m) => {
            tracing::warn!(error = %m, "停止命令下发失败");
            respond(
                502,
                Code::ChannelInteraction as i64,
                "停止命令下发失败，请重试",
                request_id,
            )
        }
        chargepilot_mod_charging::StopError::CutoffUnavailable(_) => {
            tracing::warn!(error = %err, "停止请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "停止请求暂不可用",
                request_id,
            )
        }
        chargepilot_mod_charging::StopError::Storage(_) => {
            tracing::warn!(error = %err, "停止请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "停止请求暂不可用",
                request_id,
            )
        }
    }
}

// 4291/3001/2018/5000 已入 Code 码表（票 17 缺口补齐）；直发路径逐步
// 迁 ApiError 构造时由响应测试锁定码值与状态不变。
const _: () = {
    assert!(Code::LoginRateLimited as i64 == 4291);
    assert!(Code::ChannelInteraction as i64 == 3001);
    assert!(Code::QueryExpired as i64 == 2018);
    assert!(Code::InternalError as i64 == 5000);
};

/// WalletError → HTTP 响应（票 18：从 views/wallet 门面收编，充值桥跨域消费）。
pub fn wallet_error(e: chargepilot_mod_wallet::WalletError, rid: &str) -> Response {
    use chargepilot_mod_wallet::WalletError as E;
    match &e {
        E::Forbidden => crate::respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        E::Frozen => crate::respond(409, Code::Conflict as i64, "钱包已冻结，请等待审核", rid),
        E::InsufficientBalance => crate::respond(409, Code::Conflict as i64, "可用余额不足", rid),
        E::Identity => crate::respond(409, Code::Conflict as i64, "请求号冲突或钱包已冻结", rid),
        E::RequestConflict => crate::respond(
            409,
            Code::Conflict as i64,
            "申请冲突或原充值可退额度不足",
            rid,
        ),
        E::PrepayNotConfigured | E::PrepayUnavailable => crate::respond(
            502,
            Code::ChannelInteraction as i64,
            "支付渠道暂不可用，申请已保留",
            rid,
        ),
        E::NotFound => crate::respond(404, Code::NotFound as i64, "记录不存在", rid),
        E::Inactive | E::InvalidBalance | E::Storage(_) => {
            tracing::warn!(error = %e, "钱包请求处理失败");
            let api = chargepilot_platform::error::ApiError::from(anyhow::anyhow!(e.to_string()));
            httpx::error(&api, rid)
        }
    }
}
