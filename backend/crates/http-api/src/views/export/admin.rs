//! 导出域后台门面（票 18 冷区批：原 admin_export.rs 归位）。

use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_export::ExportError;
use chargepilot_platform::error::Code;
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

// 导出配置（票 16）：由装配根从统一配置的导出组构造，经 ApiDeps 持有；
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

fn rid_of(headers: &HeaderMap) -> String {
    httpx::request_id_from(headers)
}

/// 导出错误 → HTTP（旧版 failure 映射：400/403/409/410/其他存储）。
fn export_error(e: ExportError, rid: &str) -> Response {
    match &e {
        ExportError::Input(m) => crate::respond(400, Code::BadRequest as i64, m, rid),
        ExportError::Forbidden(m) => crate::respond(403, Code::Forbidden as i64, m, rid),
        ExportError::Credentials => crate::respond(
            401,
            Code::Unauthorized as i64,
            "登录已失效，请重新登录",
            rid,
        ),
        ExportError::Download { message, expired } => {
            if *expired {
                crate::respond(410, Code::ResourceExpired as i64, message, rid)
            } else {
                crate::respond(409, Code::Conflict as i64, message, rid)
            }
        }
        ExportError::NotFound => {
            crate::respond(404, Code::NotFound as i64, "记录不存在或已删除", rid)
        }
        ExportError::Conflict => crate::respond(409, Code::Conflict as i64, "导出请求冲突", rid),
        ExportError::Storage(_) => {
            tracing::warn!(error = %e, "导出请求处理失败");
            crate::respond(
                503,
                Code::ServiceUnavailable as i64,
                "数据暂时无法读取或保存，请稍后重试",
                rid,
            )
        }
    }
}

#[derive(Deserialize)]
struct TaskListQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    status: Option<String>,
}

async fn list_tasks(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
    Query(q): Query<TaskListQuery>,
) -> Response {
    let rid = rid_of(&headers);
    match chargepilot_mod_export::list_tasks(
        &deps.db,
        &guard.profile,
        q.status.as_deref().unwrap_or(""),
        q.page.unwrap_or(1).max(1),
        q.page_size.unwrap_or(20).clamp(1, 100),
    )
    .await
    {
        Ok((items, total)) => httpx::ok(
            json!({"items": items, "total": total,
                   "page": q.page.unwrap_or(1).max(1),
                   "page_size": q.page_size.unwrap_or(20).clamp(1, 100)}),
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
    // 创建审计使用请求头请求号，不补造关联号（旧版语义）。
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
