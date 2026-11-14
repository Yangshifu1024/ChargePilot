//! 发布与继承：发布幂等（request_id + 整请求 payload_hash + 同操作者）、站点行锁串行化、
//! 版本基线（含停用记录的最新版本）、停旧插新（version=expected+1）。继承 = 删除设备自有覆盖。
//! 单体内不再经网关 HTTP：目标设备行直接在库内核对。

use crate::{PricingError, PricingResult};
use chargepilot_domain::charging_scheme::Scheme;
use chargepilot_mod_ops::Profile;
use chargepilot_mod_ops::repo;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;
use std::sync::Arc;

#[derive(Deserialize, serde::Serialize)]
pub struct PublishInput {
    pub request_id: String,
    pub station_id: u64,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub template_id: Option<u64>,
    #[serde(default)]
    pub template_version: Option<u64>,
    pub expected_version: u64,
    /// 直接内联方案（不带模板发布时使用）。
    #[serde(default)]
    pub scheme: Option<Scheme>,
}

#[derive(Deserialize)]
pub struct InheritInput {
    pub device_id: String,
    pub expected_version: u64,
}

fn payload_hash(input: &PublishInput) -> String {
    let raw = serde_json::to_string(input).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 发布：站点行锁 → 幂等回执 → 模板/内联方案 → 校验 → 版本基线 → 停旧插新 → 回执 → 审计。
/// 单事务提交；不下发任何设备指令（旧版语义）。
pub async fn publish(
    pool: &MySqlPool,
    actor: &Profile,
    input: PublishInput,
    ip: &str,
    request_id: &str,
) -> PricingResult<(u64, u64, bool)> {
    if input.station_id == 0 {
        return Err(PricingError::InvalidScheme("请选择发布目标站点".into()));
    }
    let hash = payload_hash(&input);
    let mut tx = pool.begin().await?;
    // 1. 站点行锁（站点级串行化点，旧版顺序）
    let station: Option<u64> = sqlx::query_scalar(
        "SELECT `id` FROM `station` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(input.station_id)
    .fetch_optional(&mut *tx)
    .await?;
    let station = station.ok_or(PricingError::NotFound)?;
    let _ = station;
    // 2. 操作者锁内复核（含范围：站点级发布要求站点在范围内；单实例本轮按全量放行，
    //    scope 表为空=不受限；受限账号待数据范围路由应用时一并启用）
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(PricingError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "pricing.rule.create")
    {
        return Err(PricingError::Forbidden);
    }
    // 3. 幂等回执
    let existing: Option<(u64, String, u64, u64)> = sqlx::query_as(
        "SELECT `actor_id`, `payload_hash`, `rule_id`, `version` FROM `pricing_publication` WHERE `request_id` = ?",
    )
    .bind(&input.request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((prev_actor, prev_hash, rule_id, version)) = existing {
        if prev_actor == actor.id && prev_hash == hash {
            return Ok((rule_id, version, true)); // 重放：返回首次结果
        }
        return Err(PricingError::PublishMismatch);
    }
    // 4. 解析方案：模板（SHARE 锁 + 模板版本核对）或内联
    let scheme: Scheme = match (input.template_id, input.scheme.as_ref()) {
        (Some(tid), _) => {
            if let Some(expect_tv) = input.template_version {
                let tv: Option<u64> = sqlx::query_scalar(
                    "SELECT `version` FROM `pricing_template` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR SHARE",
                )
                .bind(tid)
                .fetch_optional(&mut *tx)
                .await?;
                let tv = tv.ok_or(PricingError::NotFound)?;
                if tv != expect_tv {
                    return Err(PricingError::VersionConflict);
                }
            }
            let spec: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT `spec_json` FROM `pricing_template` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR SHARE",
            )
            .bind(tid)
            .fetch_optional(&mut *tx)
            .await?;
            let spec: crate::SpecWrapper =
                serde_json::from_value(spec.ok_or(PricingError::NotFound)?)
                    .map_err(|e| PricingError::InvalidScheme(e.to_string()))?;
            spec.scheme
                .ok_or_else(|| PricingError::InvalidScheme("模板缺少有效方案".into()))?
        }
        (None, Some(s)) => s.clone(),
        (None, None) => return Err(PricingError::InvalidScheme("请提供模板或内联方案".into())),
    };
    scheme
        .clone()
        .normalized()
        .validate()
        .map_err(|e| PricingError::InvalidScheme(e.0))?;
    // 5. 版本基线：含停用记录的最新版本（旧版语义），expected 核对
    let baseline: u64 = current_rule_version(&mut tx, input.station_id, input.device_id.as_deref())
        .await
        .map_err(PricingError::from)?;
    if baseline != input.expected_version {
        return Err(PricingError::VersionConflict);
    }
    // 6. 停用目标旧 active 规则
    match input.device_id.as_deref() {
        Some(device_id) => {
            sqlx::query(
                "UPDATE `pricing_rule` SET `status` = 'disabled' WHERE `station_id` = ? AND `device_id` = ? AND `status` = 'active' AND `deleted_at` IS NULL",
            )
            .bind(input.station_id)
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE `pricing_rule` SET `status` = 'disabled' WHERE `station_id` = ? AND `device_id` IS NULL AND `status` = 'active' AND `deleted_at` IS NULL",
            )
            .bind(input.station_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    // 7. 插入新 active 规则
    let rule_id = sqlx::query_scalar::<_, u64>(
        "SELECT CAST(COALESCE(MAX(`id`), 0) + 1 AS UNSIGNED) FROM `pricing_rule`",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(PricingError::from)? as u64;
    let version = baseline + 1;
    sqlx::query(
        "INSERT INTO `pricing_rule` (`id`, `template_id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`, `channel`) \
         VALUES (?, ?, ?, ?, ?, ?, 'active', ?, 'default')",
    )
    .bind(rule_id)
    .bind(input.template_id)
    .bind(&scheme.name)
    .bind(input.station_id)
    .bind(input.device_id.as_deref().filter(|d| !d.is_empty()))
    .bind(version)
    .bind(serde_json::to_value(crate::SpecWrapper::of(scheme.clone())).map_err(|e| PricingError::Storage(e.into()))?)
    .execute(&mut *tx)
    .await?;
    // 8. 发布回执 + 审计（同事务）
    sqlx::query("INSERT INTO `pricing_publication` (`request_id`, `actor_id`, `payload_hash`, `rule_id`, `version`) VALUES (?, ?, ?, ?, ?)")
        .bind(&input.request_id)
        .bind(actor.id)
        .bind(&hash)
        .bind(rule_id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    let after = json!({"rule_id": rule_id, "station_id": input.station_id, "device_id": input.device_id, "version": version});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "pricing_rule",
        "publish",
        "pricing_rule",
        &rule_id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(PricingError::from)?;
    tx.commit().await.map_err(PricingError::from)?;
    Ok((rule_id, version, false))
}

/// 继承 = 删除设备自有覆盖（停用设备级 active 规则，不复制数据，旧版语义）。
pub async fn inherit_station_rule(
    pool: &MySqlPool,
    actor: &Profile,
    station_id: u64,
    input: InheritInput,
    ip: &str,
    request_id: &str,
) -> PricingResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT `id` FROM `station` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE")
        .bind(station_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PricingError::NotFound)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(PricingError::from)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "pricing.rule.update")
    {
        return Err(PricingError::Forbidden);
    }
    let baseline = current_rule_version(&mut tx, station_id, Some(&input.device_id))
        .await
        .map_err(PricingError::from)?;
    if baseline != input.expected_version {
        return Err(PricingError::VersionConflict);
    }
    sqlx::query(
        "UPDATE `pricing_rule` SET `status` = 'disabled' WHERE `station_id` = ? AND `device_id` = ? AND `status` = 'active' AND `deleted_at` IS NULL",
    )
    .bind(station_id)
    .bind(&input.device_id)
    .execute(&mut *tx)
    .await?;
    let after = json!({"station_id": station_id, "device_id": input.device_id, "inherited": true});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        "pricing_rule",
        "inherit",
        "pricing_rule",
        &input.device_id,
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(PricingError::from)?;
    tx.commit().await.map_err(PricingError::from)?;
    Ok(())
}

/// 目标（站点或设备）的最新规则版本（含停用记录，旧版语义）。
pub async fn current_rule_version(
    tx: &mut sqlx::MySqlConnection,
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

/// 生效方案查询（站点详情页）：设备自有 active 优先，否则站点默认（inherited 标记）。
pub struct EffectiveScheme {
    pub scheme: Option<Scheme>,
    pub version: u64,
    pub rule_id: Option<u64>,
    pub inherited: bool,
}

pub async fn effective_scheme(
    pool: &MySqlPool,
    station_id: u64,
    device_id: Option<&str>,
) -> PricingResult<EffectiveScheme> {
    let row: Option<(u64, u64, serde_json::Value)> = match device_id {
        Some(d) => {
            sqlx::query_as(
                "SELECT `id`, `version`, `spec_json` FROM `pricing_rule` \
                 WHERE `station_id` = ? AND `device_id` = ? AND `status` = 'active' AND `deleted_at` IS NULL \
                 ORDER BY `version` DESC, `id` DESC LIMIT 1",
            )
            .bind(station_id)
            .bind(d)
            .fetch_optional(pool)
            .await?
        }
        None => None,
    };
    let (rule_id, version, spec) = match row {
        Some(r) => (Some(r.0), r.1, r.2),
        None => {
            let r: Option<(u64, u64, serde_json::Value)> = sqlx::query_as(
                "SELECT `id`, `version`, `spec_json` FROM `pricing_rule` \
                 WHERE `station_id` = ? AND `device_id` IS NULL AND `status` = 'active' AND `deleted_at` IS NULL \
                 ORDER BY `version` DESC, `id` DESC LIMIT 1",
            )
            .bind(station_id)
            .fetch_optional(pool)
            .await?;
            match r {
                Some(r) => (Some(r.0), r.1, r.2),
                None => {
                    // 无任何规则：版本基线仍返回（旧版语义：含停用记录的最新版本）
                    let mut tx = pool.begin().await.map_err(PricingError::from)?;
                    let v = current_rule_version(&mut tx, station_id, device_id)
                        .await
                        .map_err(PricingError::from)?;
                    tx.rollback().await.ok();
                    return Ok(EffectiveScheme {
                        scheme: None,
                        version: v,
                        rule_id: None,
                        inherited: false,
                    });
                }
            }
        }
    };
    let wrapped: crate::SpecWrapper =
        serde_json::from_value(spec).map_err(|e| PricingError::InvalidScheme(e.to_string()))?;
    // 显式分支替代 is_some+unwrap：None 时无从判断设备级继承，直接视为不继承。
    let inherited = match device_id.as_ref() {
        Some(d) => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM `pricing_rule` WHERE `station_id` = ? AND `device_id` = ? AND `status` = 'active' AND `deleted_at` IS NULL",
            )
            .bind(station_id)
            .bind(d)
            .fetch_one(pool)
            .await
            .map_err(PricingError::from)?
                == 0
        }
        None => false,
    };
    Ok(EffectiveScheme {
        scheme: wrapped.scheme,
        version,
        rule_id,
        inherited,
    })
}

/// Spec 的借名包装（templates 与 publication 共用同一 serde 结构）。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SpecWrapper {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<Scheme>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SpecWrapper {
    pub fn of(scheme: Scheme) -> Self {
        Self {
            scheme: Some(scheme),
            extra: serde_json::Map::new(),
        }
    }
}

pub type SharedAllocator = Arc<chargepilot_platform::ids::SegmentAllocator>;
