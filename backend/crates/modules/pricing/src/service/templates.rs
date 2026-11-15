//! 计费模板 CRUD 用例：单事务「锁模板行 → 版本核对 → 写入 → 审计」，更新版本 +1。

use crate::repo::templates;
use crate::{PricingResult, error::PricingError};
use chargepilot_domain::charging::scheme::{Scheme, Spec};
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// 列表：spec 解析失败的历史模板静默跳过，返回 {id, name, remark, scheme, version, status}。
pub async fn list_templates(pool: &MySqlPool) -> PricingResult<Vec<serde_json::Value>> {
    let rows = templates::list_active_rows(pool).await?;
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

/// 创建模板（spec_json 存 Spec 包装）。
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
    templates::insert_template_tx(
        &mut tx,
        id,
        &scheme.name,
        &scheme.remark,
        &serde_json::to_value(&spec)?,
        &serde_json::to_value(&scheme.display)?,
    )
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

/// 更新模板：FOR UPDATE 锁行 → expected_version 核对 → 写入并 version+1。
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
    let (current_version, before_spec) = templates::lock_template_tx(&mut tx, template_id)
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
    templates::update_template_tx(
        &mut tx,
        template_id,
        &scheme.name,
        &scheme.remark,
        &serde_json::to_value(&spec).map_err(PricingError::from)?,
        new_version,
    )
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

/// 状态切换 / 软删（DELETE 固定写 disabled+deleted_at）。
/// 状态切换参数即现有契约字段，不引入参数对象。
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
    let (current_version, _) = templates::lock_template_tx(&mut tx, template_id)
        .await?
        .ok_or(PricingError::NotFound)?;
    if current_version != expected_version {
        return Err(PricingError::VersionConflict);
    }
    if delete {
        templates::delete_template_tx(&mut tx, template_id).await?;
    } else {
        let s = status.ok_or_else(|| PricingError::InvalidScheme("状态无效".into()))?;
        if s != "active" && s != "disabled" {
            return Err(PricingError::InvalidScheme("状态无效".into()));
        }
        templates::set_status_tx(&mut tx, template_id, s).await?;
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
