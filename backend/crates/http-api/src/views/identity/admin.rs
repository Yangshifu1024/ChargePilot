//! 身份域后台门面（票 19 热区批：原 admin_users_cards.rs 的用户查询切片）。

//! 后台用户与在线卡路由（对齐旧版 admin/user 装配：charge_users + online_cards）。
//! 用户列表/详情/充值页与在线卡列表要求 user.read；卡绑定与状态变更要求
//! online_card.manage。路由层只做参数解析与状态码映射，角色与权限的
//! 最终判定在各服务函数内复核（与旧版 actor 实时权限语义一致）。

use crate::errormap::ops_error;
use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_identity::admin_query::AdminUserError;
use chargepilot_mod_ops::OpsError;
use chargepilot_platform::httpx;
use std::collections::HashMap;
use std::sync::Arc;

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

/// 400/1005 参数错误（旧版 httpapi.BadRequest）。
fn bad_request(message: &str, rid: &str) -> Response {
    ops_error(OpsError::Input(message.to_string()), rid)
}

/// 解析通用分页参数（旧版 ReadPaging 语义）：page 缺省 1、合法 1–1000000，
/// page_size 缺省 20、合法 1–100；空值取缺省，其余非法分别报固定文案。
fn read_paging(q: &HashMap<String, String>, rid: &str) -> Result<(u32, u32), Response> {
    let parse = |name: &str, fallback: u32, max: u32, message: &str| -> Result<u32, Response> {
        match q.get(name).map(String::as_str) {
            None | Some("") => Ok(fallback),
            Some(raw) => raw
                .parse::<u32>()
                .ok()
                .filter(|v| (1..=max).contains(v))
                .ok_or_else(|| bad_request(message, rid)),
        }
    };
    let page = parse("page", 1, 1_000_000, "分页参数无效")?;
    let page_size = parse("page_size", 20, 100, "分页参数无效：page_size 为 1–100")?;
    Ok((page, page_size))
}

/// 路径 ID 必须为正整数（旧版 AdminPathID）。
fn path_id(raw: &str, rid: &str) -> Result<u64, Response> {
    raw.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or_else(|| bad_request("ID 必须为正整数", rid))
}

/// 用户查询错误映射（旧版 AdminResourceFailure 的身份侧子集）。
fn user_error(e: AdminUserError, rid: &str) -> Response {
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

async fn list_users(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "user.read", &rid) {
        return resp;
    }
    let (page, page_size) = match read_paging(&q, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let keyword = q.get("keyword").map(|s| s.trim()).unwrap_or("");
    if keyword.chars().count() > 128 {
        return bad_request("关键词过长或状态无效", &rid);
    }
    let status = q.get("status").map(String::as_str).unwrap_or("");
    // 状态白名单与旧版 ReadAdminPage 一致：active frozen。
    if !status.is_empty() && status != "active" && status != "frozen" {
        return bad_request("关键词过长或状态无效", &rid);
    }
    match chargepilot_mod_identity::admin_query::list_users(
        &deps.db,
        &guard.profile,
        page,
        page_size,
        keyword,
        status,
    )
    .await
    {
        Ok(paged) => httpx::ok(paged, &rid),
        Err(e) => user_error(e, &rid),
    }
}

async fn user_detail(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "user.read", &rid) {
        return resp;
    }
    let user_id = match path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match chargepilot_mod_identity::admin_query::user_detail(&deps.db, &guard.profile, user_id)
        .await
    {
        Ok(detail) => httpx::ok(detail, &rid),
        Err(e) => user_error(e, &rid),
    }
}

async fn user_recharges(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "user.read", &rid) {
        return resp;
    }
    let user_id = match path_id(&id, &rid) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // 旧版逐参数校验：仅支持单值非空 page 和 page_size，其他参数一律拒绝。
    let mut page = 1u32;
    let mut page_size = 20u32;
    if let Some(q) = raw.as_deref() {
        let mut seen: Vec<&str> = Vec::new();
        for pair in q.split('&') {
            let Some((name, value)) = pair.split_once('=') else {
                return bad_request("仅支持单值 page 和 page_size 分页参数", &rid);
            };
            if (name != "page" && name != "page_size") || value.is_empty() || seen.contains(&name) {
                return bad_request("仅支持单值 page 和 page_size 分页参数", &rid);
            }
            seen.push(name);
            let parsed = value.parse::<u32>();
            if name == "page" {
                match parsed {
                    Ok(v) if (1..=1_000_000).contains(&v) => page = v,
                    _ => return bad_request("分页参数无效", &rid),
                }
            } else {
                match parsed {
                    Ok(v) if (1..=100).contains(&v) => page_size = v,
                    _ => return bad_request("分页参数无效：page_size 为 1–100", &rid),
                }
            }
        }
    }
    if let Err(resp) =
        chargepilot_mod_identity::admin_query::require_user_exists(&deps.db, user_id).await
    {
        return user_error(resp, &rid);
    }
    match chargepilot_mod_wallet::recharge_history::admin_recharges(
        &deps.db, user_id, page, page_size,
    )
    .await
    {
        Ok(paged) => httpx::ok(paged, &rid),
        Err(e) => {
            tracing::warn!(error = %e, "充值记录查询失败");
            ops_error(OpsError::Storage(anyhow::anyhow!(e.to_string())), &rid)
        }
    }
}
/// 用户查询路由（票 19 热区批）。
pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route("/api/v1/admin/users", axum::routing::get(list_users))
        .route("/api/v1/admin/users/{id}", axum::routing::get(user_detail))
        .route(
            "/api/v1/admin/users/{id}/recharges",
            axum::routing::get(user_recharges),
        )
}
