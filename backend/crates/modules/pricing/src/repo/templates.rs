//! pricing_template 表访问（蓝图第五章 repo 层）。

use chargepilot_domain::pricing::PricingTemplateStatus;
use sqlx::{MySqlConnection, MySqlPool};

use crate::error::PricingError;

/// 模板行（spec_json 直出；解析失败的历史模板由调用方静默跳过）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct TemplateRow {
    pub id: u64,
    pub name: String,
    pub remark: String,
    pub spec: serde_json::Value,
    pub status: String,
    pub version: u64,
}

/// 在用模板列表（Active，倒序）。
pub async fn list_active_rows(pool: &MySqlPool) -> Result<Vec<TemplateRow>, PricingError> {
    Ok(sqlx::query_as::<_, TemplateRow>(
        "SELECT `id`, `name`, `remark`, `spec_json` AS `spec`, `status`, `version` \
         FROM `pricing_template` WHERE `deleted_at` IS NULL AND `status` = ? ORDER BY `id` DESC",
    )
    .bind(PricingTemplateStatus::Active)
    .fetch_all(pool)
    .await?)
}

/// 创建模板（version=1）。
pub async fn insert_template_tx(
    tx: &mut MySqlConnection,
    id: u64,
    name: &str,
    remark: &str,
    spec_json: &serde_json::Value,
    display_json: &serde_json::Value,
) -> Result<(), PricingError> {
    sqlx::query(
        "INSERT INTO `pricing_template` (`id`, `name`, `remark`, `spec_json`, `display_json`, `status`, `version`) VALUES (?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(id)
    .bind(name)
    .bind(remark)
    .bind(spec_json)
    .bind(display_json)
    .bind(PricingTemplateStatus::Active)
    .execute(tx)
    .await?;
    Ok(())
}

/// 锁模板行读当前版本与 spec（FOR UPDATE）；已删除行视为不存在。
pub async fn lock_template_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
) -> Result<Option<(u64, Option<serde_json::Value>)>, PricingError> {
    sqlx::query_as(
        "SELECT `version`, `spec_json` FROM `pricing_template` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(template_id)
    .fetch_optional(tx)
    .await
    .map_err(PricingError::from)
}

/// 更新模板内容并推进版本。
pub async fn update_template_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
    name: &str,
    remark: &str,
    spec_json: &serde_json::Value,
    version: u64,
) -> Result<(), PricingError> {
    sqlx::query(
        "UPDATE `pricing_template` SET `name` = ?, `remark` = ?, `spec_json` = ?, `version` = ? WHERE `id` = ?",
    )
    .bind(name)
    .bind(remark)
    .bind(spec_json)
    .bind(version)
    .bind(template_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// 软删：状态置 disabled 并写 deleted_at。
pub async fn delete_template_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
) -> Result<(), PricingError> {
    sqlx::query("UPDATE `pricing_template` SET `status` = ?, `deleted_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(PricingTemplateStatus::Disabled)
        .bind(template_id)
        .execute(tx)
        .await?;
    Ok(())
}

/// 状态切换（active/disabled）。
pub async fn set_status_tx(
    tx: &mut MySqlConnection,
    template_id: u64,
    status: &str,
) -> Result<(), PricingError> {
    sqlx::query("UPDATE `pricing_template` SET `status` = ? WHERE `id` = ?")
        .bind(status)
        .bind(template_id)
        .execute(tx)
        .await?;
    Ok(())
}
