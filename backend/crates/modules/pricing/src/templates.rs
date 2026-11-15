//! 计费模板 CRUD：单事务「锁模板行 → 版本核对 → 写入 → 审计」，更新版本 +1（旧版语义）。

use crate::{PricingError, PricingResult};
use chargepilot_domain::charging::scheme::{Scheme, Spec};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct TemplateRow {
    pub id: u64,
    pub name: String,
    pub remark: String,
    pub spec: serde_json::Value,
    pub status: String,
    pub version: u64,
}

/// 列表：spec 解析失败的历史模板静默跳过（旧版语义），返回 {id, name, remark, scheme, version, status}。
pub async fn list_templates(pool: &MySqlPool) -> PricingResult<Vec<serde_json::Value>> {
    let rows = sqlx::query_as::<_, TemplateRow>(
        "SELECT `id`, `name`, `remark`, `spec_json` AS `spec`, `status`, `version` \
         FROM `pricing_template` WHERE `deleted_at` IS NULL AND `status` = ? ORDER BY `id` DESC",
    )
    .bind(chargepilot_domain::pricing::PricingTemplateStatus::Active)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let spec: Option<Spec> = serde_json::from_value(r.spec.clone()).ok();
            let scheme = spec.and_then(|s| s.scheme)?;
            Some(json!({
                "id": r.id, "name": scheme.name, "remark": scheme.remark,
                "scheme": scheme, "version": r.version, "status": r.status,
            }))
        })
        .collect())
}

#[derive(Deserialize)]
pub struct SchemeInput {
    pub scheme: Scheme,
    /// 乐观锁版本（创建忽略；更新必须与已存版本一致）。
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// 创建模板（spec_json 存 Spec 包装，与旧版结构一致）。
pub async fn create_template(
    pool: &MySqlPool,
    actor: &Profile,
    input: SchemeInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> PricingResult<(u64, u64)> {
    let scheme = input.scheme.normalized();
    scheme
        .validate()
        .map_err(|e| PricingError::InvalidScheme(e.0))?;
    let id = allocator.next_id().await.map_err(PricingError::from)?;
    let spec = Spec {
        scheme: Some(scheme.clone()),
        extra: serde_json::Map::new(),
    };
    let mut tx = pool.begin().await?;
    repo::lock_customer_admin_role_tx(&mut tx).await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id).await?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "pricing.rule.create")
    {
        return Err(PricingError::Forbidden);
    }
    sqlx::query(
        "INSERT INTO `pricing_template` (`id`, `name`, `remark`, `spec_json`, `display_json`, `status`, `version`) VALUES (?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(id)
    .bind(&scheme.name)
    .bind(&scheme.remark)
    .bind(serde_json::to_value(&spec)?)
    .bind(serde_json::to_value(&scheme.display)?)
    .bind(chargepilot_domain::pricing::PricingTemplateStatus::Active)
    .execute(&mut *tx)
    .await?;
    let after = json!({"id": id, "name": scheme.name, "version": 1});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "pricing_template",
        "create",
        "pricing_template",
        &id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok((id, 1))
}

/// 更新模板：FOR UPDATE 锁行 → expected_version 核对 → 写入并 version+1（旧版语义）。
pub async fn update_template(
    pool: &MySqlPool,
    actor: &Profile,
    template_id: u64,
    input: SchemeInput,
    ip: &str,
    request_id: &str,
) -> PricingResult<(u64, u64)> {
    let scheme = input.scheme.normalized();
    scheme
        .validate()
        .map_err(|e| PricingError::InvalidScheme(e.0))?;
    let expected = input
        .expected_version
        .ok_or_else(|| PricingError::VersionConflict)?;
    let mut tx = pool.begin().await?;
    repo::lock_customer_admin_role_tx(&mut tx).await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id).await?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "pricing.rule.update")
    {
        return Err(PricingError::Forbidden);
    }
    let (current_version, before_spec): (u64, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT `version`, `spec_json` FROM `pricing_template` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(PricingError::NotFound)?;
    if current_version != expected {
        return Err(PricingError::VersionConflict);
    }
    let spec = Spec {
        scheme: Some(scheme.clone()),
        extra: serde_json::Map::new(),
    };
    let new_version = current_version + 1;
    sqlx::query(
        "UPDATE `pricing_template` SET `name` = ?, `remark` = ?, `spec_json` = ?, `version` = ? WHERE `id` = ?",
    )
    .bind(&scheme.name)
    .bind(&scheme.remark)
    .bind(serde_json::to_value(&spec).map_err(PricingError::from)?)
    .bind(new_version)
    .bind(template_id)
    .execute(&mut *tx)
    .await?;
    let after = json!({"id": template_id, "name": scheme.name, "version": new_version});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "pricing_template",
        "update",
        "pricing_template",
        &template_id.to_string(),
        &before_spec.unwrap_or(Value::Null),
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok((template_id, new_version))
}

/// 状态切换 / 软删（DELETE 固定写 disabled+deleted_at，旧版语义）。
/// 状态切换参数即旧版契约字段，不引入参数对象。
#[allow(clippy::too_many_arguments)]
pub async fn set_template_status(
    pool: &MySqlPool,
    actor: &Profile,
    template_id: u64,
    expected_version: u64,
    status: Option<&str>,
    delete: bool,
    ip: &str,
    request_id: &str,
) -> PricingResult<u64> {
    let mut tx = pool.begin().await?;
    repo::lock_customer_admin_role_tx(&mut tx).await?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id).await?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "pricing.rule.update")
    {
        return Err(PricingError::Forbidden);
    }
    let (current_version, _): (u64, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT `version`, `spec_json` FROM `pricing_template` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(template_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(PricingError::NotFound)?;
    if current_version != expected_version {
        return Err(PricingError::VersionConflict);
    }
    if delete {
        sqlx::query("UPDATE `pricing_template` SET `status` = ?, `deleted_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
            .bind(chargepilot_domain::pricing::PricingTemplateStatus::Disabled)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    } else {
        let s = status.ok_or_else(|| PricingError::InvalidScheme("状态无效".into()))?;
        if s != "active" && s != "disabled" {
            return Err(PricingError::InvalidScheme("状态无效".into()));
        }
        sqlx::query("UPDATE `pricing_template` SET `status` = ? WHERE `id` = ?")
            .bind(s)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    let after = json!({"id": template_id, "deleted": delete});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "pricing_template",
        "update",
        "pricing_template",
        &template_id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(template_id)
}
