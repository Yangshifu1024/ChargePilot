//! 后台管理路由：账号管理、角色管理、公告、仪表盘（路径/权限码/响应形状对齐旧版）。

use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use crate::errormap::ops_error;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_ops::OpsError;
use chargepilot_mod_ops::management::{self, CreateAdminInput, Page, RoleInput, UpdateAdminInput};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        // 账号管理
        .route(
            "/api/v1/admin/admin-users",
            axum::routing::get(list_admin_users).post(create_admin_user),
        )
        .route(
            "/api/v1/admin/admin-users/{id}",
            axum::routing::put(update_admin_user).delete(delete_admin_user),
        )
        .route(
            "/api/v1/admin/admin-users/{id}/unlock",
            axum::routing::post(unlock_admin_user),
        )
        .route(
            "/api/v1/admin/admin-users/{id}/reset-password",
            axum::routing::post(reset_password),
        )
        // 角色管理
        .route(
            "/api/v1/admin/roles",
            axum::routing::get(list_roles).post(create_role),
        )
        .route(
            "/api/v1/admin/permissions",
            axum::routing::get(list_permissions),
        )
        .route(
            "/api/v1/admin/roles/{id}",
            axum::routing::put(update_role).delete(delete_role),
        )
        // 公告与仪表盘
        .route(
            "/api/v1/admin/announcements",
            axum::routing::get(list_announcements).post(publish_announcement),
        )
        .route("/api/v1/admin/dashboard", axum::routing::get(dashboard))
}

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

fn page_query(query: &PagingQuery) -> Result<Page, Response> {
    Page::sanitized(query.page.unwrap_or(1), query.page_size.unwrap_or(20))
        .map_err(|e| ops_error(OpsError::bad_input(e.to_string()), ""))
}

#[derive(Deserialize)]
struct PagingQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    keyword: Option<String>,
    status: Option<String>,
}

async fn list_admin_users(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<PagingQuery>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.read", &rid) {
        return resp;
    }
    let page = match page_query(&q) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let status_filter = q
        .status
        .as_deref()
        .filter(|s| ["active", "disabled", "locked"].contains(s));
    match management::list_admin_users(&deps.db, page, q.keyword.as_deref(), status_filter).await {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total, "page": page.page, "page_size": page.page_size}),
            &rid,
        ),
        Err(e) => ops_error(OpsError::from(e), &rid),
    }
}

async fn create_admin_user(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.create", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(
            400,
            1005,
            "用户名须为 3–64 位英文/数字，密码须为 12–72 字节，并选择有效角色",
            &rid,
        );
    };
    let input = CreateAdminInput {
        username: v["username"].as_str().unwrap_or_default().to_string(),
        display_name: v["display_name"].as_str().map(str::to_string),
        password: v["password"].as_str().unwrap_or_default().to_string(),
        role_id: v["role_id"].as_u64().unwrap_or(0),
        phone: v["phone"].as_str().map(str::to_string),
    };
    let ip = real_ip(&headers);
    match management::create_admin(&deps.db, &guard.profile, input, &ip, &rid, &deps.allocator)
        .await
    {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn update_admin_user(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.update", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, 1005, "没有可更新的字段", &rid);
    };
    let input = UpdateAdminInput {
        display_name: v
            .get("display_name")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        phone: v.get("phone").and_then(|x| x.as_str()).map(str::to_string),
        email: v.get("email").and_then(|x| x.as_str()).map(str::to_string),
        role_id: v.get("role_id").and_then(|x| x.as_u64()),
        status: v.get("status").and_then(|x| x.as_str()).map(str::to_string),
    };
    match management::update_admin(
        &deps.db,
        &guard.profile,
        id,
        input,
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok((id, sessions_revoked)) => httpx::ok(
            json!({"id": id, "sessions_revoked": sessions_revoked}),
            &rid,
        ),
        Err(e) => ops_error(e, &rid),
    }
}

async fn delete_admin_user(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.delete", &rid) {
        return resp;
    }
    match management::delete_admin(&deps.db, &guard.profile, id, &real_ip(&headers), &rid).await {
        Ok(id) => httpx::ok(json!({"id": id, "deleted": true}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn unlock_admin_user(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.update", &rid) {
        return resp;
    }
    match management::unlock_admin(&deps.db, &guard.profile, id, &real_ip(&headers), &rid).await {
        Ok(id) => httpx::ok(json!({"id": id, "unlocked": true}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn reset_password(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.reset_password", &rid) {
        return resp;
    }
    let new_password = body
        .and_then(|b| {
            b.0.get("new_password")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    match management::reset_admin_password(
        &deps.db,
        &guard.profile,
        id,
        &new_password,
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id, "sessions_revoked": true}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn list_roles(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.read", &rid) {
        return resp;
    }
    match management::list_roles(&deps.db).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn list_permissions(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "admin_user.read", &rid) {
        return resp;
    }
    match management::list_permissions(&deps.db).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn create_role(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "role.create", &rid) {
        return resp;
    }
    let Some(input) = role_input(body.map(|b| b.0)) else {
        return crate::respond(
            400,
            1005,
            "角色无效：编码须为小写字母开头的 3–64 位，名称必填，权限须非空且只能从自己已持有的权限中选择",
            &rid,
        );
    };
    match management::create_role(&deps.db, &guard.profile, input, &real_ip(&headers), &rid).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn update_role(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "role.update", &rid) {
        return resp;
    }
    let Some(input) = role_input(body.map(|b| b.0)) else {
        return crate::respond(
            400,
            1005,
            "角色无效：编码须为小写字母开头的 3–64 位，名称必填，权限须非空且只能从自己已持有的权限中选择",
            &rid,
        );
    };
    match management::update_role(
        &deps.db,
        &guard.profile,
        id,
        input,
        &real_ip(&headers),
        &rid,
    )
    .await
    {
        Ok((id, permissions_locked)) => httpx::ok(
            json!({"id": id, "permissions_locked": permissions_locked}),
            &rid,
        ),
        Err(e) => ops_error(e, &rid),
    }
}

async fn delete_role(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "role.delete", &rid) {
        return resp;
    }
    match management::delete_role(&deps.db, &guard.profile, id, &real_ip(&headers), &rid).await {
        Ok(id) => httpx::ok(json!({"id": id}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

fn role_input(v: Option<Value>) -> Option<RoleInput> {
    let v = v?;
    Some(RoleInput {
        code: v["code"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        description: v["description"].as_str().map(str::to_string),
        permissions: v["permissions"]
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
    })
}

async fn list_announcements(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "announcement.read", &rid) {
        return resp;
    }
    match chargepilot_mod_ops::announcement::list(&deps.db).await {
        Ok(items) => httpx::ok(json!({"items": items}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn publish_announcement(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "announcement.create", &rid) {
        return resp;
    }
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(400, 1005, "请填写标题、内容、有效时间和公告目标", &rid);
    };
    let input = chargepilot_mod_ops::announcement::PublishInput {
        title: v["title"].as_str().unwrap_or_default().to_string(),
        content: v["content"].as_str().unwrap_or_default().to_string(),
        scope: v["scope"].as_str().unwrap_or("global").to_string(),
        target_ids: v["target_ids"].as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        start_at: v["start_at"].as_str().unwrap_or_default().to_string(),
        end_at: v["end_at"].as_str().map(str::to_string),
    };
    match chargepilot_mod_ops::announcement::publish(
        &deps.db,
        &guard.profile,
        input,
        &real_ip(&headers),
        &rid,
        &deps.allocator,
    )
    .await
    {
        Ok(id) => httpx::ok(json!({"id": id, "status": "published"}), &rid),
        Err(e) => ops_error(e, &rid),
    }
}

async fn dashboard(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    if let Some(resp) = deny(&guard, "dashboard.read", &rid) {
        return resp;
    }
    match chargepilot_mod_ops::dashboard::load(&deps.db).await {
        Ok(data) => match serde_json::to_value(&data) {
            Ok(v) => httpx::ok(v, &rid),
            Err(_) => crate::respond(503, 5003, "仪表盘数据暂不可用", &rid),
        },
        Err(_) => crate::respond(503, 5003, "仪表盘数据暂不可用", &rid),
    }
}

fn real_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}
