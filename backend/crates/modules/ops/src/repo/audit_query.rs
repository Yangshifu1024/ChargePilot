//! 审计日志分页查询。
//! 只读：业务写入经各业务事务的 `repo::append_audit_tx` / `write_auth_audit`。
//! 模块/动作/目标类型精确匹配，操作人按登录名 LIKE（`!` 转义）筛选，按 ID 倒序。
//! JSON 快照 CAST AS CHAR 读出，避免 []byte base64 或 JSON 解析失败。

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// 审计列表筛选条件，空串/None 表示该维度不限。
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// 所属模块，如 auth、feedback、admin_user。
    pub module: String,
    /// 动作，如 login、reply、update。
    pub action: String,
    /// 操作人登录名筛选（LIKE，转义通配符）。
    pub actor: String,
    /// 目标类型筛选。
    pub target_type: String,
    /// 起始时间（含），RFC 3339 解析后的 UTC。
    pub from: Option<DateTime<Utc>>,
    /// 截止时间（含）。
    pub to: Option<DateTime<Utc>>,
}

/// `!` 转义 LIKE 关键词中的百分号和下划线。
fn like_pattern(value: &str) -> String {
    let escaped = value
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_");
    format!("%{escaped}%")
}

/// 分页查询审计记录（已由 HTTP 层完成 audit.read 实时权限与分页校验）。
/// 返回 (行, 总数)；行 JSON 与后台审计列表 API 一致，
/// target_type/target_id/request_id/client_ip/before_json/after_json 可空（NULL→null）。
pub async fn list(
    pool: &MySqlPool,
    q: &AuditQuery,
    page: u32,
    page_size: u32,
) -> anyhow::Result<(Vec<Value>, i64)> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `audit_log` WHERE (? = '' OR `module` = ?) \
         AND (? = '' OR `action` = ?) AND (? = '' OR `actor_name` LIKE ? ESCAPE '!') \
         AND (? = '' OR `target_type` = ?) \
         AND (? IS NULL OR `created_at` >= ?) AND (? IS NULL OR `created_at` <= ?)",
    )
    .bind(&q.module)
    .bind(&q.module)
    .bind(&q.action)
    .bind(&q.action)
    .bind(&q.actor)
    .bind(like_pattern(&q.actor))
    .bind(&q.target_type)
    .bind(&q.target_type)
    .bind(q.from)
    .bind(q.from)
    .bind(q.to)
    .bind(q.to)
    .fetch_one(pool)
    .await?;
    // 审计行投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type AuditRow = (
        u64,
        u64,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT `id`, `actor_id`, `actor_name`, `module`, `action`, \
                `target_type`, `target_id`, `request_id`, `client_ip`, \
                CAST(`before_json` AS CHAR) AS `before_json`, \
                CAST(`after_json` AS CHAR) AS `after_json`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `audit_log` WHERE (? = '' OR `module` = ?) \
         AND (? = '' OR `action` = ?) AND (? = '' OR `actor_name` LIKE ? ESCAPE '!') \
         AND (? = '' OR `target_type` = ?) \
         AND (? IS NULL OR `created_at` >= ?) AND (? IS NULL OR `created_at` <= ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(&q.module)
    .bind(&q.module)
    .bind(&q.action)
    .bind(&q.action)
    .bind(&q.actor)
    .bind(like_pattern(&q.actor))
    .bind(&q.target_type)
    .bind(&q.target_type)
    .bind(q.from)
    .bind(q.from)
    .bind(q.to)
    .bind(q.to)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(
            |(
                id,
                actor_id,
                actor_name,
                module,
                action,
                target_type,
                target_id,
                request_id,
                client_ip,
                before_json,
                after_json,
                created_at,
            )| {
                json!({
                    "id": id,
                    "actor_id": actor_id,
                    "actor_name": actor_name.unwrap_or_default(),
                    "module": module,
                    "action": action,
                    "target_type": target_type,
                    "target_id": target_id,
                    "request_id": request_id,
                    "client_ip": client_ip,
                    "before_json": before_json,
                    "after_json": after_json,
                    "created_at": created_at,
                })
            },
        )
        .collect();
    Ok((items, total))
}
