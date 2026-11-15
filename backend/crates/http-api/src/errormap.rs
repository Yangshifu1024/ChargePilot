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

// ── views 局部映射收编（架构评审候选 1 步骤③）────────────────────────
// 21 个文件局部 *_error 函数归位本文件：映射语义与「域错误 → HTTP 契约」
// 的唯一枢纽同址，新增错误变体不再散落。状态/码值/文案逐字保留；
// device_error 合并 user/admin 两份（仅差一条 Storage 臂日志，HTTP 输出
// 一致，取带日志版）；marketing 的 deny 文案差异保留在原文件。

use axum::http::HeaderMap;

use chargepilot_mod_card::CardAdminError;
use chargepilot_mod_device::{DeviceError, ScanError};
use chargepilot_mod_export::ExportError;
use chargepilot_mod_identity::{AdminUserError, PhoneError};
use chargepilot_mod_marketing::{CouponError, CouponQueryError};
use chargepilot_mod_ops::MfaError;
use chargepilot_mod_order::FeedbackError;
use chargepilot_mod_payment::CheckoutError;
use chargepilot_mod_pricing::PricingError;
use chargepilot_mod_settlement::{
    InvoiceAdminError, RefundAdminError, ReviewError, UserInvoiceError,
};

/// 从请求头解析 request_id（原 19 份 views 局部 rid_of 副本的唯一归宿）。
pub fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// DeviceError → HTTP（user/admin 门面共用；Storage 臂带日志）。
pub fn device_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => respond(400, Code::BadRequest as i64, msg, rid),
        DeviceError::Forbidden => respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        DeviceError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        DeviceError::Conflict(msg) => respond(409, Code::Conflict as i64, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "设备域存储操作失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// DeviceError → HTTP（设备能力查询专用文案，与 device_error 并存）。
pub fn capability_error(e: DeviceError, rid: &str) -> Response {
    match &e {
        DeviceError::Input(msg) => respond(400, Code::BadRequest as i64, msg, rid),
        DeviceError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        DeviceError::Forbidden => respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        DeviceError::Conflict(msg) => respond(409, Code::Conflict as i64, msg, rid),
        DeviceError::Storage(_) => {
            tracing::warn!(cause = %e, "设备能力查询失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// ScanError → HTTP（匿名扫码；英文文案对齐旧版 scan API）。
pub fn scan_error(e: ScanError, rid: &str) -> Response {
    match e {
        ScanError::NotFound => respond(404, Code::NotFound as i64, "scan code not found", rid),
        ScanError::Storage(err) => {
            tracing::warn!(cause = %err, "扫码查询失败");
            respond(
                503,
                Code::DependencyFailed as i64,
                "device lookup unavailable",
                rid,
            )
        }
    }
}

/// FeedbackError → HTTP（后台工单 + 用户端提交共用）。
pub fn feedback_error(e: FeedbackError, rid: &str) -> Response {
    match &e {
        FeedbackError::Input(msg) => respond(400, Code::BadRequest as i64, msg, rid),
        FeedbackError::Conflict => respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        FeedbackError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        FeedbackError::Forbidden => respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        FeedbackError::Storage(_) => {
            tracing::warn!(cause = %e, "反馈请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// FeedbackSubmitError → HTTP（用户端提交的两段阶段契约，文案逐字保留）。
/// 引用解析阶段（业务单号/数字主键 → 订单 ID）。
pub fn feedback_resolve_error(
    e: chargepilot_mod_order::FeedbackSubmitError,
    rid: &str,
) -> Response {
    use chargepilot_mod_order::FeedbackSubmitError as E;
    match e {
        E::Input(msg) => respond(400, Code::BadRequest as i64, &msg, rid),
        E::NotFound => respond(404, Code::NotFound as i64, "订单不存在", rid),
        e @ (E::OrderNotReady | E::Exists | E::Storage(_)) => {
            tracing::warn!(error = %e, "订单引用解析失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法保存，请稍后重试",
                rid,
            )
        }
    }
}

/// FeedbackSubmitError → HTTP（提交阶段；解析失败已在 resolve 阶段返回）。
pub fn feedback_submit_error(e: chargepilot_mod_order::FeedbackSubmitError, rid: &str) -> Response {
    use chargepilot_mod_order::FeedbackSubmitError as E;
    match e {
        E::Input(msg) => respond(400, Code::BadRequest as i64, &msg, rid),
        E::OrderNotReady => respond(404, Code::NotFound as i64, "订单不存在或不可评价", rid),
        E::Exists => respond(409, Code::Conflict as i64, "该订单已评价", rid),
        E::NotFound => respond(404, Code::NotFound as i64, "订单不存在", rid),
        E::Storage(e) => {
            tracing::warn!(error = %e, "反馈提交失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "反馈提交失败，请稍后重试",
                rid,
            )
        }
    }
}

/// 后台订单查询的 sqlx 错误直映（原 order/admin storage_error）。
pub fn order_storage_error(e: sqlx::Error, rid: &str) -> Response {
    tracing::warn!(cause = %e, "后台订单查询失败");
    ops_error(OpsError::Storage(anyhow::anyhow!(e.to_string())), rid)
}

/// CardAdminError → HTTP（Input 文案由调用点传入：各端点参数契约不同）。
pub fn card_error(e: CardAdminError, input_message: &str, rid: &str) -> Response {
    match &e {
        CardAdminError::Input => respond(400, Code::BadRequest as i64, input_message, rid),
        CardAdminError::Forbidden => ops_error(OpsError::Permission, rid),
        CardAdminError::Conflict(msg) => ops_error(OpsError::Conflict(msg.clone()), rid),
        CardAdminError::NotFound => {
            ops_error(OpsError::NotFound("记录不存在或已删除".to_string()), rid)
        }
        CardAdminError::Storage(_) => {
            tracing::warn!(error = %e, "在线卡管理失败");
            ops_error(OpsError::Storage(anyhow::anyhow!(e.to_string())), rid)
        }
    }
}

/// PhoneError → HTTP（C 端手机号绑定，文案对齐旧版 PhoneAPI）。
pub fn phone_error(e: PhoneError, rid: &str) -> Response {
    match &e {
        PhoneError::Authorization => respond(
            400,
            Code::BadRequest as i64,
            "请使用微信手机号授权凭证",
            rid,
        ),
        PhoneError::Unavailable => respond(
            503,
            Code::ServiceUnavailable as i64,
            "手机号授权暂不可用",
            rid,
        ),
        PhoneError::Exchange => respond(
            400,
            Code::NotFound as i64,
            "手机号授权失败，请重新授权",
            rid,
        ),
        PhoneError::Input => respond(
            400,
            Code::BadRequest as i64,
            "请输入有效的中国大陆手机号",
            rid,
        ),
        PhoneError::Taken => respond(409, Code::Conflict as i64, "该手机号已绑定其他账号", rid),
        PhoneError::Storage(_) => {
            tracing::warn!(error = %e, "手机号保存失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法保存，请稍后重试",
                rid,
            )
        }
    }
}

/// AdminUserError → HTTP（后台用户查询，原 identity/admin user_error）。
pub fn admin_user_error(e: AdminUserError, rid: &str) -> Response {
    match &e {
        AdminUserError::Forbidden => ops_error(OpsError::Permission, rid),
        AdminUserError::NotFound => {
            ops_error(OpsError::NotFound("记录不存在或已删除".to_string()), rid)
        }
        AdminUserError::Storage(_) => {
            tracing::warn!(error = %e, "后台用户查询失败");
            ops_error(OpsError::Storage(anyhow::anyhow!(e.to_string())), rid)
        }
    }
}

/// InvoiceAdminError → HTTP（后台发票审核）。
pub fn invoice_admin_error(e: InvoiceAdminError, rid: &str) -> Response {
    match &e {
        InvoiceAdminError::Conflict => respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        InvoiceAdminError::NotFound => {
            respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        InvoiceAdminError::Forbidden => respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        InvoiceAdminError::Storage(m) => {
            tracing::warn!(error = %m, "发票审核处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// ReviewError → HTTP（计量核实；2009 状态冲突直发）。
pub fn meter_error(e: ReviewError, rid: &str) -> Response {
    match &e {
        ReviewError::Conflict => respond(
            409,
            2009,
            "计量核实状态已变化、原始数据不一致或审核人重复，请刷新后重试",
            rid,
        ),
        ReviewError::InvalidSource => respond(
            400,
            1005,
            "计量分段须连续覆盖原始时段、总电量一致，且各段不得跨不同费率；价格快照必须有效",
            rid,
        ),
        ReviewError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        ReviewError::Storage(m) => {
            tracing::warn!(error = %m, "计量核实处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// RefundAdminError → HTTP（人工退款；StateConflict 直发）。
pub fn refund_admin_error(e: RefundAdminError, rid: &str) -> Response {
    match &e {
        RefundAdminError::Conflict => respond(
            409,
            Code::Conflict as i64,
            "当前记录状态不允许此操作，请刷新后重试",
            rid,
        ),
        RefundAdminError::NotFound => {
            respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        RefundAdminError::NotSettled => respond(
            409,
            Code::StateConflict as i64,
            "订单尚未完成费用结算，请结算后再申请退款",
            rid,
        ),
        RefundAdminError::Storage(m) => {
            tracing::warn!(error = %m, "人工退款处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// UserInvoiceError → HTTP（C 端开票申请，文案对齐旧版 applyInvoice）。
pub fn user_invoice_error(e: &UserInvoiceError, rid: &str) -> Response {
    match e {
        UserInvoiceError::Unbilled => respond(
            409,
            Code::Conflict as i64,
            "订单尚未计费，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::OrderNotFound => respond(404, Code::NotFound as i64, "订单不存在", rid),
        UserInvoiceError::OrderLookup(_) => respond(
            503,
            Code::ServiceUnavailable as i64,
            "订单暂时无法读取",
            rid,
        ),
        UserInvoiceError::Unfinished => respond(
            409,
            Code::Conflict as i64,
            "订单尚未完成，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::ZeroFee => respond(
            409,
            Code::Conflict as i64,
            "订单金额为零，暂不能申请发票",
            rid,
        ),
        UserInvoiceError::Duplicate => {
            respond(409, Code::Conflict as i64, "该订单已申请过发票", rid)
        }
        UserInvoiceError::InvalidInput => {
            respond(400, Code::BadRequest as i64, "发票申请无效", rid)
        }
        UserInvoiceError::Storage(m) => {
            tracing::warn!(error = %m, "发票申请保存失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法保存，请稍后重试",
                rid,
            )
        }
    }
}

/// CouponQueryError → HTTP（C 端券查询：输入 400，其余 503 固定文案）。
pub fn coupon_query_error(e: CouponQueryError, rid: &str) -> Response {
    match &e {
        CouponQueryError::Input(m) => respond(400, Code::BadRequest as i64, m, rid),
        CouponQueryError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券查询处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "优惠券暂时无法读取",
                rid,
            )
        }
    }
}

/// CouponError → HTTP（营销后台；403 文案「无权执行此操作」保留域差异）。
pub fn coupon_error(e: CouponError, rid: &str) -> Response {
    match &e {
        CouponError::Input(m) => respond(400, Code::BadRequest as i64, m, rid),
        CouponError::Forbidden => respond(403, Code::Forbidden as i64, "无权执行此操作", rid),
        CouponError::Conflict(m) => respond(409, Code::Conflict as i64, m, rid),
        CouponError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        CouponError::Storage(_) => {
            tracing::warn!(error = %e, "优惠券请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// CheckoutError → HTTP（下单链，文案对齐旧版 CheckoutError Kind）。
pub fn checkout_error(e: CheckoutError, rid: &str) -> Response {
    match &e {
        CheckoutError::InvalidInput => respond(400, Code::BadRequest as i64, "请求参数无效", rid),
        CheckoutError::NotFound => respond(404, Code::NotFound as i64, "设备或端口不存在", rid),
        CheckoutError::Paused => respond(409, Code::Conflict as i64, "设备已停用", rid),
        CheckoutError::PortBusy => respond(409, Code::Conflict as i64, "端口已被占用", rid),
        CheckoutError::OfferUnavailable => respond(409, Code::Conflict as i64, "套餐不可售", rid),
        CheckoutError::Pricing => respond(409, Code::Conflict as i64, "计费方案不可用", rid),
        CheckoutError::Conflict => respond(409, Code::Conflict as i64, "支付申请冲突", rid),
        CheckoutError::Provider => {
            respond(502, Code::ChannelInteraction as i64, "支付渠道不可用", rid)
        }
        CheckoutError::Storage(m) => {
            tracing::warn!(error = %m, "下单处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "下单暂不可用，请稍后重试",
                rid,
            )
        }
    }
}

/// WalletError → HTTP（钱包风控列表专用契约，与 wallet_error 并存）。
pub fn risk_error(e: chargepilot_mod_wallet::WalletError, rid: &str) -> Response {
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

/// MfaError → HTTP（后台 MFA 设置；文案对齐旧版契约）。
pub fn mfa_error(e: MfaError, rid: &str) -> Response {
    match &e {
        MfaError::Input | MfaError::InputMsg(_) => {
            respond(400, Code::BadRequest as i64, &e.to_string(), rid)
        }
        MfaError::Proof => respond(
            400,
            Code::VerificationFailed as i64,
            "密码或验证码校验失败",
            rid,
        ),
        MfaError::UsedTotp => respond(
            400,
            Code::VerificationFailed as i64,
            "该验证码已使用，请等待下一个验证码",
            rid,
        ),
        MfaError::Pending | MfaError::State | MfaError::Busy => {
            respond(409, Code::Conflict as i64, &e.to_string(), rid)
        }
        MfaError::Rate => {
            let mut resp = respond(
                429,
                Code::LoginRateLimited as i64,
                "安全验证过于频繁，请稍后重试",
                rid,
            );
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("60"));
            resp
        }
        MfaError::SelfMfa => respond(
            409,
            Code::Conflict as i64,
            "请使用本人账号的安全设置修改双因素认证",
            rid,
        ),
        MfaError::Credentials => respond(
            401,
            Code::Unauthorized as i64,
            "用户名或密码错误，或登录已失效",
            rid,
        ),
        MfaError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        MfaError::Storage(_) => {
            tracing::warn!(cause = %e, "MFA 请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// PricingError → HTTP（计费发布/模板管理）。
pub fn pricing_error(e: PricingError, rid: &str) -> Response {
    match &e {
        PricingError::InvalidScheme(msg) => respond(409, Code::Conflict as i64, msg, rid),
        PricingError::VersionConflict | PricingError::PublishMismatch => {
            respond(409, Code::Conflict as i64, &e.to_string(), rid)
        }
        PricingError::Forbidden => respond(403, Code::Forbidden as i64, "没有操作权限", rid),
        PricingError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        PricingError::RuleUnavailable => respond(409, Code::Conflict as i64, &e.to_string(), rid),
        PricingError::Storage(_) => {
            tracing::warn!(cause = %e, "pricing 存储失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// ExportError → HTTP（导出任务；Download 按过期态分派 410/409）。
pub fn export_error(e: ExportError, rid: &str) -> Response {
    match &e {
        ExportError::Input(m) => respond(400, Code::BadRequest as i64, m, rid),
        ExportError::Forbidden(m) => respond(403, Code::Forbidden as i64, m, rid),
        ExportError::Credentials => respond(
            401,
            Code::Unauthorized as i64,
            "登录已失效，请重新登录",
            rid,
        ),
        ExportError::Download { message, expired } => {
            if *expired {
                respond(410, Code::ResourceExpired as i64, message, rid)
            } else {
                respond(409, Code::Conflict as i64, message, rid)
            }
        }
        ExportError::NotFound => respond(404, Code::NotFound as i64, "记录不存在或已删除", rid),
        ExportError::Conflict => respond(409, Code::Conflict as i64, "导出请求冲突", rid),
        ExportError::Storage(_) => {
            tracing::warn!(error = %e, "导出请求处理失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

/// C 端充电查询失败视图（原 views/charging 局部枚举；错误映射唯一归宿）。
pub enum QueryFail {
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

/// QueryFail → HTTP（充电历史/详情/快照/曲线共用）。
pub fn query_failure(f: QueryFail, rid: &str) -> Response {
    match f {
        QueryFail::Input(m) => respond(400, Code::BadRequest as i64, m, rid),
        QueryFail::Conflict(m) => respond(409, Code::Conflict as i64, m, rid),
        QueryFail::Expired(m) => respond(422, Code::QueryExpired as i64, m, rid),
        QueryFail::NotFound => respond(404, Code::NotFound as i64, "充电订单不存在", rid),
        QueryFail::Storage(e) => {
            tracing::warn!(error = %e, "用户订单查询失败");
            respond(
                503,
                Code::ServiceUnavailable as i64,
                "订单暂时无法读取",
                rid,
            )
        }
    }
}
