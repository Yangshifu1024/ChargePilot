//! 事务内入队：事件与业务写入同事务提交（可靠投递的根基）。

use crate::{EventKind, EventSource};
use sqlx::MySqlConnection;

/// 在既有业务事务上写 `domain_event` 行（status=pending）。
///
/// `event_id` 是**业务幂等键**（如 `charge-ended-{order_id}-{segment}`），唯一键冲突时
/// 静默跳过——事件重放安全（at-least-once 语义下入队重复无害）。
pub fn in_tx(
    conn: &mut MySqlConnection,
    source: EventSource,
    kind: EventKind,
    event_id: &str,
    payload: &serde_json::Value,
) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send {
    let source_str = match source {
        EventSource::User => "user",
        EventSource::Admin => "admin",
        EventSource::Gateway => "gateway",
    };
    let payload = payload.clone();
    let kind = kind.as_str().to_owned();
    let event_id = event_id.to_string();
    async move {
        let result = sqlx::query(
            "INSERT IGNORE INTO `domain_event` (`event_id`, `source`, `kind`, `payload_json`) VALUES (?, ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(source_str)
        .bind(&kind)
        .bind(&payload)
        .execute(&mut *conn)
        .await?;
        if result.rows_affected() == 0 {
            tracing::debug!(event_id, "事件已存在（幂等跳过）");
        }
        Ok(())
    }
}

/// 编译期锁定首批事件类别与来源枚举可见性。
#[allow(dead_code)]
fn smoke() {
    let _ = (EventKind::ChargeEnded, EventSource::Gateway);
}
