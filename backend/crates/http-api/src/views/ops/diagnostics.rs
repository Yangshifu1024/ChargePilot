//! 运营域后台门面·诊断（票 18 冷区批：原 admin_diagnostics.rs 归位）。

use crate::guard::AdminGuard;
use crate::state::ApiDeps;
use axum::extract::State;
use axum::http::HeaderMap;
use chargepilot_platform::httpx;
use chargepilot_platform::sweeperx;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn diagnostics_router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route("/api/v1/admin/diagnostics", axum::routing::get(diagnostics))
}

/// 队列深度：各兜底扫描的消费对象现存量（与扫描器结果交叉可判健康）。
async fn queue_depths(db: &sqlx::MySqlPool) -> Value {
    async fn one(db: &sqlx::MySqlPool, sql: &str) -> u64 {
        sqlx::query_scalar::<_, u64>(sql)
            .fetch_one(db)
            .await
            .unwrap_or(0)
    }
    let domain_event_pending = one(
        db,
        "SELECT COUNT(*) FROM `domain_event` WHERE `status` = 'pending'",
    )
    .await;
    let domain_event_failed = one(
        db,
        "SELECT COUNT(*) FROM `domain_event` WHERE `status` = 'failed'",
    )
    .await;
    let billing_jobs_pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_billing_job` WHERE `status` = ?")
            .bind(chargepilot_domain::settlement::BillingJobStatus::Pending)
            .fetch_one(db)
            .await
            .unwrap_or(0);
    let commands_sent = one(
        db,
        "SELECT COUNT(*) FROM `charge_command` WHERE `status` = 'sent'",
    )
    .await;
    let stop_commands_unacked = one(
        db,
        "SELECT COUNT(*) FROM `charge_stop_command` WHERE `status` = 'sent' AND `result_reported` = 0",
    )
    .await;
    let charging_long_running = one(
        db,
        "SELECT COUNT(*) FROM `charge_order` WHERE `status` = 'charging' \
         AND `started_at` < DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 2 HOUR)",
    )
    .await;
    let exports_processing = one(
        db,
        "SELECT COUNT(*) FROM `export_task` WHERE `status` = 'processing'",
    )
    .await;
    json!({
        "domain_event_pending": domain_event_pending,
        "domain_event_failed": domain_event_failed,
        "billing_jobs_pending": billing_jobs_pending,
        "start_commands_sent": commands_sent,
        "stop_commands_unacked": stop_commands_unacked,
        "charging_over_2h": charging_long_running,
        "exports_processing": exports_processing,
    })
}

async fn diagnostics(
    State(deps): State<Arc<ApiDeps>>,
    guard: AdminGuard,
    headers: HeaderMap,
) -> axum::response::Response {
    let rid = httpx::request_id_from(&headers);
    if !guard.has("audit.read") {
        return crate::errormap::ops_error(chargepilot_mod_ops::OpsError::Permission, &rid);
    }
    let ip = headers
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    if let Err(e) = chargepilot_mod_ops::repo::write_diagnostics_audit(
        &deps.db,
        guard.profile.id,
        &guard.profile.username,
        &ip,
    )
    .await
    {
        tracing::warn!(error = %e, "诊断审计写入失败（不影响查询）");
    }
    httpx::ok(
        json!({
            "scanners": sweeperx::snapshot(),
            "queues": queue_depths(&deps.db).await,
        }),
        &rid,
    )
}
