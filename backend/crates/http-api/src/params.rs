//! HTTP 参数提取器（票 17）：分页/游标/状态过滤的横切校验唯一归宿。
//!
//! 双轨上界与状态过滤双语义按调用点保留（票 15/16 定论，防止误判漂移）：
//! - 游标分页：`after_id` + `limit`（1–2000，默认 1000，domain::limits 锚点）；
//! - 状态过滤：静默降级（透传给服务层兜底）与参数错误（白名单拒绝）两种
//!   既有语义并存，`StatusFilter` 按调用点选择。

use axum::response::Response;
use chargepilot_domain::limits as _limits;
use chargepilot_platform::error::Code;

use crate::errormap::respond;

/// 游标分页参数（过程/时间线等 keyset 场景）。
#[derive(Debug, Clone, Copy)]
pub struct CursorParams {
    pub after_id: u64,
    pub limit: u32,
}

impl CursorParams {
    /// 解析并校验（after_id 空串/非数字、limit 越界均 400/1005）。
    /// `messages.0` 覆盖通用错误文案、`messages.1` 覆盖 limit 范围文案
    /// （既有调用点的字面量契约逐字保留）。
    pub fn parse(
        after_id: Option<&str>,
        limit: Option<&str>,
        rid: &str,
        messages: (&str, &str),
    ) -> Result<Self, Response> {
        let after_id = match after_id {
            None => 0,
            Some("") => return Err(respond(400, Code::BadRequest as i64, messages.0, rid)),
            Some(raw) => match raw.parse::<u64>() {
                Ok(v) => v,
                Err(_) => return Err(respond(400, Code::BadRequest as i64, messages.1, rid)),
            },
        };
        let limit = match limit {
            None => u32::try_from(_limits::CURSOR_LIMIT_DEFAULT).unwrap_or(1_000),
            Some("") => return Err(respond(400, Code::BadRequest as i64, messages.0, rid)),
            Some(raw) => match raw.parse::<u64>() {
                Ok(v) if _limits::CURSOR_LIMIT.contains(&v) => v as u32,
                _ => return Err(respond(400, Code::BadRequest as i64, messages.1, rid)),
            },
        };
        Ok(Self { after_id, limit })
    }
}

/// 状态过滤参数：两种既有语义按调用点选择（票 16/15 定论）。
pub struct StatusFilter;

impl StatusFilter {
    /// 静默降级语义：透传给服务层（服务层对未知值有自己的兜底行为）。
    pub fn passthrough(status: Option<&str>) -> &str {
        status.unwrap_or("")
    }

    /// 参数错误语义：非白名单值返回 400/1005（文案由调用点保留）。
    pub fn allowed(
        status: Option<&str>,
        whitelist: &[&str],
        rid: &str,
        message: &str,
    ) -> Result<(), Response> {
        match status {
            None | Some("") => Ok(()),
            Some(s) if whitelist.contains(&s) => Ok(()),
            Some(_) => Err(respond(400, Code::BadRequest as i64, message, rid)),
        }
    }
}
