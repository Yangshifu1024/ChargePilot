//! 后台导出路由（路径/权限码对齐旧版 admin/export 装配）：
//! 列表与详情 finance.read；受理与下载 export.create；下载以文件体直出。

use crate::admin_guard::AdminGuard;
use crate::deps::ApiDeps;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_mod_export::{ExportConfig, ExportError};
use chargepilot_platform::httpx;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// 导出配置（旧版 ExportService 的 ExportDir/MaxRows，进程装配读取环境变量）。
pub fn config_from_env() -> ExportConfig {
    ExportConfig {
        export_dir: std::env::var("EXPORT_DIR").unwrap_or_default(),
        max_rows: std::env::var("EXPORT_MAX_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    }
}

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
        ExportError::Input(m) => crate::respond(400, 1005, m, rid),
        ExportError::Forbidden(m) => crate::respond(403, 1003, m, rid),
        ExportError::Credentials => crate::respond(401, 1001, "登录已失效，请重新登录", rid),
        ExportError::Download { message, expired } => {
            if *expired {
                crate::respond(410, 1007, message, rid)
            } else {
                crate::respond(409, 2009, message, rid)
            }
        }
        ExportError::NotFound => crate::respond(404, 1004, "记录不存在或已删除", rid),
        ExportError::Conflict => crate::respond(409, 2009, "导出请求冲突", rid),
        ExportError::Storage(_) => {
            tracing::warn!(error = %e, "导出请求处理失败");
            crate::respond(503, 5003, "数据暂时无法读取或保存，请稍后重试", rid)
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
    match chargepilot_mod_export::list_resources(&deps.db, &guard.profile, &config_from_env()).await
    {
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
        return crate::respond(400, 1005, "请提供 UUID 格式的请求号", &rid);
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
        &config_from_env(),
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
    match chargepilot_mod_export::download(&deps.db, &guard.profile, id, &config_from_env()).await {
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
                .unwrap_or_else(|_| crate::respond(500, 5000, "响应构造失败", ""))
        }
        Err(e) => export_error(e, &rid),
    }
}
