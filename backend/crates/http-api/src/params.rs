//! HTTP 参数提取器：分页/游标/状态过滤的横切校验唯一归宿。
//!
//! 双轨上界与状态过滤双语义按调用点保留：
//! - 页码分页：[`UserPaging`]（C 端轨）/ [`AdminPaging`]（后台轨），校验唯一
//! 经 `domain::page::PageRequest`（蓝图 F6），越界 400 沿用既有字段级文案；
//! - 游标分页：`after_id` + `limit`（1–2000，默认 1000，domain::limits 锚点）；
//! - 状态过滤：静默降级（透传给服务层兜底）与参数错误（白名单拒绝）两种
//! 既有语义并存，`StatusFilter` 按调用点选择。

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::Uri;
use axum::response::Response;
use chargepilot_domain::limits as _limits;
use chargepilot_domain::page::{PageBoundsField, PageRequest};
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;

use crate::errormap::respond;

// ── 页码分页双轨提取器（F6：domain::page 唯一校验入口） ────────────────

/// C 端轨分页参数（页码 1..=10 万，`domain::limits::USER_PAGE_MAX`）。
pub struct UserPaging {
    pub page: u32,
    pub page_size: u32,
}

/// 后台轨分页参数（页码 1..=100 万，`domain::limits::ADMIN_PAGE_MAX`）。
pub struct AdminPaging {
    pub page: u32,
    pub page_size: u32,
}

/// 字段级越界文案（与 ops 域 Page::sanitized、identity read_paging 的
/// 既有字面量逐字一致，None 用于非数字场景的通用文案）。
fn paging_message(field: Option<PageBoundsField>) -> &'static str {
    match field {
        Some(PageBoundsField::PageSize) => "分页参数无效：page_size 为 1–100",
        _ => "分页参数无效",
    }
}

/// 从查询串解析 page/page_size：缺省 1/20；空串视同缺省（identity
/// read_paging 语义）；非数字按字段级文案 400。
fn raw_paging(uri: &Uri, rid: &str) -> Result<(i64, i64), Response> {
    let mut page = 1i64;
    let mut page_size = 20i64;
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let (slot, name): (&mut i64, &str) = match key {
                "page" => (&mut page, "page"),
                "page_size" => (&mut page_size, "page_size"),
                _ => continue,
            };
            if value.is_empty() {
                continue;
            }
            match value.parse::<i64>() {
                Ok(v) => *slot = v,
                Err(_) => {
                    let message = match name {
                        "page_size" => paging_message(Some(PageBoundsField::PageSize)),
                        _ => paging_message(None),
                    };
                    return Err(respond(400, Code::BadRequest as i64, message, rid));
                }
            }
        }
    }
    Ok((page, page_size))
}

impl UserPaging {
    fn parse(uri: &Uri, rid: &str) -> Result<Self, Response> {
        let (page, page_size) = raw_paging(uri, rid)?;
        match PageRequest::user(page, page_size) {
            Ok(p) => Ok(Self {
                page: p.page,
                page_size: p.page_size,
            }),
            Err(field) => Err(respond(
                400,
                Code::BadRequest as i64,
                paging_message(Some(field)),
                rid,
            )),
        }
    }
}

impl AdminPaging {
    fn parse(uri: &Uri, rid: &str) -> Result<Self, Response> {
        let (page, page_size) = raw_paging(uri, rid)?;
        match PageRequest::admin(page, page_size) {
            Ok(p) => Ok(Self {
                page: p.page,
                page_size: p.page_size,
            }),
            Err(field) => Err(respond(
                400,
                Code::BadRequest as i64,
                paging_message(Some(field)),
                rid,
            )),
        }
    }
}

impl FromRequestParts<Arc<super::state::ApiDeps>> for UserPaging {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &Arc<super::state::ApiDeps>,
    ) -> Result<Self, Self::Rejection> {
        let rid = httpx::request_id_from(&parts.headers);
        Self::parse(&parts.uri, &rid)
    }
}

impl FromRequestParts<Arc<super::state::ApiDeps>> for AdminPaging {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &Arc<super::state::ApiDeps>,
    ) -> Result<Self, Self::Rejection> {
        let rid = httpx::request_id_from(&parts.headers);
        Self::parse(&parts.uri, &rid)
    }
}

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

/// 状态过滤参数：两种既有语义按调用点选择。
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
