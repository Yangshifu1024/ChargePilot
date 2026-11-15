//! pricing_rule / pricing_publication / pricing_template 表访问（蓝图第五章 repo 层）。

use crate::error::PricingError;
use chargepilot_domain::pricing::{PricingChannel, PricingRuleStatus, PricingTemplateStatus};
use serde_json::Value;
use sqlx::{MySqlConnection, MySqlPool};

/// 站点行锁（发布/继承的串行化点）。
pub async fn lock_station_tx(
    tx: &mut MySqlConnection,
    station_id: u64,
) -> Result<Option<u64>, PricingError> {
    sqlx::query_scalar(
        "SELECT `id` FROM `station` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(station_id)
    .fetch_optional(tx)
    .await
    .map_err(PricingError::from)
}

/// 发布幂等回执查询（request_id 唯一）。
pub async fn find_publication_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
) -> Result<Option<(u64, String, u64, u64)>, PricingError> {
    sqlx::query_as(
        "SELECT `actor_id`, `payload_hash`, `rule_id`, `version` FROM `pricing_publication` WHERE `request_id` = ?",
    )
    .bind(request_id)
    .fetch_optional(tx)
    .await
    .map_err(PricingError::from)
}

/// SHARE 锁读在用模板版本。
pub async fn lock_template_version_share_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
) -> Result<Option<u64>, PricingError> {
    sqlx::query_scalar(
        "SELECT `version` FROM `pricing_template` WHERE `id` = ? AND `status` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(template_id)
    .bind(PricingTemplateStatus::Active)
    .fetch_optional(tx)
    .await
    .map_err(PricingError::from)
}

/// SHARE 锁读在用模板 spec。
pub async fn template_spec_share_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
) -> Result<Option<Value>, PricingError> {
    sqlx::query_scalar(
        "SELECT `spec_json` FROM `pricing_template` WHERE `id` = ? AND `status` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(template_id)
    .bind(PricingTemplateStatus::Active)
    .fetch_optional(tx)
    .await
    .map_err(PricingError::from)
}

/// 目标（站点或设备）的最新规则版本（含停用记录）。
pub async fn current_rule_version(
    tx: &mut MySqlConnection,
    station_id: u64,
    device_id: Option<&str>,
) -> anyhow::Result<u64> {
    let v: u64 = match device_id {
        Some(d) if !d.is_empty() => {
            sqlx::query_scalar(
                "SELECT CAST(COALESCE(MAX(`version`), 0) AS UNSIGNED) FROM `pricing_rule` WHERE `station_id` = ? AND `device_id` = ? AND `deleted_at` IS NULL",
            )
            .bind(station_id)
            .bind(d)
            .fetch_one(&mut *tx)
            .await?
        }
        _ => {
            sqlx::query_scalar(
                "SELECT CAST(COALESCE(MAX(`version`), 0) AS UNSIGNED) FROM `pricing_rule` WHERE `station_id` = ? AND `device_id` IS NULL AND `deleted_at` IS NULL",
            )
            .bind(station_id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    Ok(v)
}

/// 停用目标当前 active 规则（发布停旧插新的「停旧」与设备继承同一条语句）。
pub async fn disable_active_rule_tx(
    tx: &mut MySqlConnection,
    station_id: u64,
    device_id: Option<&str>,
) -> Result<(), PricingError> {
    match device_id {
        Some(device_id) => {
            sqlx::query(
                "UPDATE `pricing_rule` SET `status` = ? WHERE `station_id` = ? AND `device_id` = ? AND `status` = ? AND `deleted_at` IS NULL",
            )
            .bind(PricingRuleStatus::Disabled)
            .bind(station_id)
            .bind(device_id)
            .bind(PricingRuleStatus::Active)
            .execute(tx)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE `pricing_rule` SET `status` = ? WHERE `station_id` = ? AND `device_id` IS NULL AND `status` = ? AND `deleted_at` IS NULL",
            )
            .bind(PricingRuleStatus::Disabled)
            .bind(station_id)
            .bind(PricingRuleStatus::Active)
            .execute(tx)
            .await?;
        }
    }
    Ok(())
}

/// 规则主键取号（MAX(id)+1，站点规则表低频写入）。
pub async fn next_rule_id_tx(tx: &mut MySqlConnection) -> Result<u64, PricingError> {
    sqlx::query_scalar::<_, u64>(
        "SELECT CAST(COALESCE(MAX(`id`), 0) + 1 AS UNSIGNED) FROM `pricing_rule`",
    )
    .fetch_one(tx)
    .await
    .map_err(PricingError::from)
}

/// 插入新 active 规则。
#[allow(clippy::too_many_arguments)]
pub async fn insert_rule_tx(
    tx: &mut MySqlConnection,
    rule_id: u64,
    template_id: Option<u64>,
    name: &str,
    station_id: u64,
    device_id: Option<&str>,
    version: u64,
    spec_json: &Value,
) -> Result<(), PricingError> {
    sqlx::query(
        "INSERT INTO `pricing_rule` (`id`, `template_id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`, `channel`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(rule_id)
    .bind(template_id)
    .bind(name)
    .bind(station_id)
    .bind(device_id)
    .bind(version)
    .bind(PricingRuleStatus::Active)
    .bind(spec_json)
    .bind(PricingChannel::Default)
    .execute(tx)
    .await?;
    Ok(())
}

/// 写发布幂等回执。
pub async fn insert_publication_tx(
    tx: &mut MySqlConnection,
    request_id: &str,
    actor_id: u64,
    payload_hash: &str,
    rule_id: u64,
    version: u64,
) -> Result<(), PricingError> {
    sqlx::query("INSERT INTO `pricing_publication` (`request_id`, `actor_id`, `payload_hash`, `rule_id`, `version`) VALUES (?, ?, ?, ?, ?)")
        .bind(request_id)
        .bind(actor_id)
        .bind(payload_hash)
        .bind(rule_id)
        .bind(version)
        .execute(tx)
        .await?;
    Ok(())
}

/// 设备自有 active 规则行（最新版本优先）。
pub async fn device_active_rule(
    pool: &MySqlPool,
    station_id: u64,
    device_id: &str,
) -> Result<Option<(u64, u64, Value)>, PricingError> {
    sqlx::query_as(
        "SELECT `id`, `version`, `spec_json` FROM `pricing_rule` \
         WHERE `station_id` = ? AND `device_id` = ? AND `status` = ? AND `deleted_at` IS NULL \
         ORDER BY `version` DESC, `id` DESC LIMIT 1",
    )
    .bind(station_id)
    .bind(device_id)
    .bind(PricingRuleStatus::Active)
    .fetch_optional(pool)
    .await
    .map_err(PricingError::from)
}

/// 站点默认 active 规则行（device_id IS NULL）。
pub async fn station_active_rule(
    pool: &MySqlPool,
    station_id: u64,
) -> Result<Option<(u64, u64, Value)>, PricingError> {
    sqlx::query_as(
        "SELECT `id`, `version`, `spec_json` FROM `pricing_rule` \
         WHERE `station_id` = ? AND `device_id` IS NULL AND `status` = ? AND `deleted_at` IS NULL \
         ORDER BY `version` DESC, `id` DESC LIMIT 1",
    )
    .bind(station_id)
    .bind(PricingRuleStatus::Active)
    .fetch_optional(pool)
    .await
    .map_err(PricingError::from)
}

/// 设备自有 active 规则存在计数（继承标记判定）。
pub async fn device_active_rule_count(
    pool: &MySqlPool,
    station_id: u64,
    device_id: &str,
) -> Result<i64, PricingError> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM `pricing_rule` WHERE `station_id` = ? AND `device_id` = ? AND `status` = ? AND `deleted_at` IS NULL",
    )
    .bind(station_id)
    .bind(device_id)
    .bind(PricingRuleStatus::Active)
    .fetch_one(pool)
    .await
    .map_err(PricingError::from)
}
