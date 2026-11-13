//! 计量匹配：交易入口按端口的 station+device 找生效方案（设备自有优先于站点默认，
//! 同目标取最新版本），spec 校验通过后交调用方冻结快照（阶段 04 事务内保存）。

use crate::publication::SpecWrapper;
use crate::{PricingError, PricingResult};
use chargepilot_domain::charging_scheme::Scheme;
use sqlx::MySqlPool;

#[derive(Debug)]
pub struct ActiveRule {
    pub rule_id: u64,
    pub version: u64,
    pub scheme: Scheme,
}

/// 查生效规则。device_id 非空时设备自有优先（device_id IS NULL 排后），无规则 → RuleUnavailable。
pub async fn active_device_rule(
    pool: &MySqlPool,
    station_id: u64,
    device_id: Option<&str>,
) -> PricingResult<ActiveRule> {
    let row: Option<(u64, u64, serde_json::Value)> = match device_id {
        Some(d) if !d.is_empty() => {
            sqlx::query_as(
                "SELECT pr.`id`, pr.`version`, pr.`spec_json` \
                 FROM `pricing_rule` pr JOIN `station` s ON s.`id` = pr.`station_id` \
                 WHERE pr.`station_id` = ? AND s.`status` = 'active' AND s.`deleted_at` IS NULL \
                   AND pr.`status` = 'active' AND pr.`deleted_at` IS NULL \
                   AND (pr.`effective_from` IS NULL OR pr.`effective_from` <= NOW(3)) \
                   AND (pr.`effective_to` IS NULL OR pr.`effective_to` > NOW(3)) \
                   AND (pr.`device_id` = ? OR pr.`device_id` IS NULL) \
                 ORDER BY pr.`device_id` IS NULL, pr.`version` DESC, pr.`id` DESC LIMIT 1",
            )
            .bind(station_id)
            .bind(d)
            .fetch_optional(pool)
            .await?
        }
        _ => {
            sqlx::query_as(
                "SELECT pr.`id`, pr.`version`, pr.`spec_json` \
                 FROM `pricing_rule` pr JOIN `station` s ON s.`id` = pr.`station_id` \
                 WHERE pr.`station_id` = ? AND s.`status` = 'active' AND s.`deleted_at` IS NULL \
                   AND pr.`status` = 'active' AND pr.`deleted_at` IS NULL \
                   AND (pr.`effective_from` IS NULL OR pr.`effective_from` <= NOW(3)) \
                   AND (pr.`effective_to` IS NULL OR pr.`effective_to` > NOW(3)) \
                   AND pr.`device_id` IS NULL \
                 ORDER BY pr.`version` DESC, pr.`id` DESC LIMIT 1",
            )
            .bind(station_id)
            .fetch_optional(pool)
            .await?
        }
    };
    let Some((rule_id, version, spec)) = row else {
        return Err(PricingError::RuleUnavailable);
    };
    let wrapped: SpecWrapper =
        serde_json::from_value(spec).map_err(|e| PricingError::InvalidScheme(e.to_string()))?;
    let scheme = wrapped
        .scheme
        .ok_or_else(|| PricingError::InvalidScheme("规则缺少有效方案".into()))?;
    let validated = scheme.clone().normalized();
    validated
        .validate()
        .map_err(|e| PricingError::InvalidScheme(e.0))?;
    Ok(ActiveRule {
        rule_id,
        version,
        scheme,
    })
}
