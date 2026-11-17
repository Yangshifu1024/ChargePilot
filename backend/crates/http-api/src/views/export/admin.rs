//! 导出域后台门面。

use crate::errormap::{export_error, rid_of};
use crate::guard::AdminGuard;
use crate::params::AdminPaging;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

// 导出配置：由装配根从统一配置的导出组构造，经 ApiDeps 持有；
// HTTP 处理器只消费，不再直读环境变量。

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new()
        .route(
            "/api/v1/admin/exports",
            axum::routing::get(list_tasks).post(create_task),
        )
        .route(
            "/api/v1/admin/exports/resources",
            axum::routing::get(list_resources),
        )
        .route(
            "/api/v1/admin/exports/{id}",
            axum::routing::get(detail_task),
        )
        .route(
            "/api/v1/admin/exports/{id}/download",
            axum::routing::get(download_task),
        )
}

/// 导出错误 → HTTP。
#[derive(Deserialize)]
struct TaskListQuery {
    status: Option<String>,
}

async fn list_tasks(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    AdminPaging { page, page_size }: AdminPaging,
    Query(q): Query<TaskListQuery>,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_export::list_tasks(
        &deps.db,
        &guard.profile,
        q.status.as_deref().unwrap_or(""),
        page.into(),
        page_size.into(),
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total,
                   "page": page,
                   "page_size": page_size}),
            &rid,
        ),
        Err(e) => export_error(e, &rid),
    }
}

async fn detail_task(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_export::detail(&deps.db, &guard.profile, id).await {
        Ok(row) => httpx::ok(serde_json::to_value(&row).unwrap_or(Value::Null), &rid),
        Err(e) => export_error(e, &rid),
    }
}

async fn list_resources(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_export::list_resources(&deps.db, &guard.profile, &deps.export).await {
        Ok(out) => httpx::ok(serde_json::to_value(&out).unwrap_or(Value::Null), &rid),
        Err(e) => export_error(e, &rid),
    }
}

async fn create_task(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let rid = rid_of(&headers);
    let Some(v) = body.map(|b| b.0) else {
        return crate::respond(
            400,
            Code::BadRequest as i64,
            "请提供 UUID 格式的请求号",
            &rid,
        );
    };
    let input = chargepilot_mod_export::CreateInput {
        request_id: v["request_id"].as_str().unwrap_or_default().to_string(),
        resource: v["resource"].as_str().unwrap_or_default().to_string(),
        format: v["format"].as_str().unwrap_or_default().to_string(),
        filter: v.get("filter").cloned().unwrap_or(Value::Null),
    };
    let ip = headers
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    // 创建审计使用请求头请求号，不补造关联号（契约约定）。
    let audit_rid = httpx::request_id_from(&headers);
    match chargepilot_mod_export::create_export(
        &deps.db,
        &guard.profile,
        &ip,
        &audit_rid,
        &input,
        &deps.export,
    )
    .await
    {
        Ok(out) => {
            if !out.message.is_empty() {
                httpx::ok(
                    json!({"task_no": out.task_no, "status": out.status, "message": out.message}),
                    &rid,
                )
            } else {
                httpx::ok(
                    json!({"task_no": out.task_no, "status": out.status,
                           "row_count": out.row_count, "expires_at": out.expires_at}),
                    &rid,
                )
            }
        }
        Err(e) => export_error(e, &rid),
    }
}

async fn download_task(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_export::download(&deps.db, &guard.profile, id, &deps.export).await {
        Ok(file) => {
            let body = axum::body::Body::from(file.content);
            axum::response::Response::builder()
                .status(200)
                .header("content-type", file.mime)
                .header(
                    "content-disposition",
                    format!("attachment; filename=\"{}\"", file.name),
                )
                .header("x-request-id", rid)
                .body(body)
                .unwrap_or_else(|_| {
                    crate::respond(500, Code::InternalError as i64, "响应构造失败", "")
                })
        }
        Err(e) => export_error(e, &rid),
    }
}
