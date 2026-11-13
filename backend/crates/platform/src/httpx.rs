//! 统一响应信封与请求编号（契约与旧版逐字段一致，属红线层）。
//!
//! 信封：`{code, message, data, request_id, trace_id, errors?}`；
//! 请求编号：沿用合法 `X-Request-ID` 头（≤128，`[A-Za-z0-9_-]`），否则生成 16 字节 hex，
//! 并原样写入响应头；响应一律 `Cache-Control: no-store`。

use crate::error::{ApiError, Code};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// 统一响应信封（字段名与旧版逐字节一致）。
#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub code: i64,
    pub message: String,
    pub data: T,
    pub request_id: String,
    pub trace_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<crate::error::FieldErr>,
}

impl<T: Serialize> Envelope<T> {
    pub fn ok(data: T, request_id: &str) -> Self {
        Self {
            code: 0,
            message: "ok".into(),
            data,
            request_id: request_id.into(),
            trace_id: request_id.into(),
            errors: Vec::new(),
        }
    }
}

/// 从请求头取（或生成）请求编号；合法字符集与旧版一致。
pub fn request_id_from(headers: &axum::http::HeaderMap) -> String {
    use rand::RngCore;
    if let Some(raw) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        let id = raw.trim();
        if !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return id.to_string();
        }
    }
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn respond(status: StatusCode, request_id: &str, body: impl Serialize) -> Response {
    let mut resp = (status, axum::Json(body)).into_response();
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

/// 200 + 业务码 0。
pub fn ok<T: Serialize>(data: T, request_id: &str) -> Response {
    respond(StatusCode::OK, request_id, Envelope::ok(data, request_id))
}

/// 按错误码映射状态码与信封（内部原因记日志，不进响应）。
pub fn error(err: &ApiError, request_id: &str) -> Response {
    if let Some(cause) = &err.cause {
        tracing::warn!(code = err.code as i64, cause = %cause, "请求处理失败");
    }
    let body = Envelope {
        code: err.code as i64,
        message: err.message.clone(),
        data: serde_json::Value::Null,
        request_id: request_id.to_string(),
        trace_id: request_id.to_string(),
        errors: err.fields.clone(),
    };
    respond(err.code.http_status(), request_id, body)
}

/// 未匹配路径的统一 404（语义与文案沿用旧版）。
pub fn not_found_route(request_id: &str) -> Response {
    error(
        &ApiError::new(Code::NotFound, "接口尚未提供，请确认服务版本"),
        request_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn ok_envelope_shape() {
        let resp = ok(serde_json::json!({"id": 1}), "abc123");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-request-id").unwrap(), "abc123");
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    }

    #[test]
    fn error_envelope_uses_frozen_codes() {
        let resp = error(&ApiError::state_conflict("端口被占用"), "rid");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = error(&ApiError::new(Code::AccountLocked, "locked"), "rid");
        assert_eq!(resp.status(), StatusCode::LOCKED);
    }

    #[test]
    fn request_id_falls_back_to_random() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(request_id_from(&headers).len(), 32);
        headers.insert("x-request-id", HeaderValue::from_static("legal-ID_1"));
        assert_eq!(request_id_from(&headers), "legal-ID_1");
        headers.insert("x-request-id", HeaderValue::from_static("bad id!"));
        assert_eq!(request_id_from(&headers).len(), 32);
    }
}
