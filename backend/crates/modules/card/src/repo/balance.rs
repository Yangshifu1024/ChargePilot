//! 卡余额事实读原语（gateway-runtime 设备链专用：余额查询应答与刷卡拒绝
//! 回显共用）。

use sqlx::MySqlPool;

/// 卡有效性事实与钱包可用余额（分）：卡缺失/停用返回 (false, 0)。
/// 可用额度取持卡人钱包（balance_cents - frozen_cents）；卡与钱包是同库
/// 跨域事实查询，卡域读自己的事实面（无 crate 依赖）。
pub async fn balance_facts(pool: &MySqlPool, card_no: &str) -> anyhow::Result<(bool, i64)> {
    let row: Option<(chargepilot_domain::card::CardStatus, Option<i64>)> = sqlx::query_as(
        "SELECT c.`status`, w.`balance_cents` - w.`frozen_cents` \
         FROM `online_card` c \
         LEFT JOIN `wallet_account` w ON w.`user_id` = c.`user_id` AND w.`deleted_at` IS NULL \
         WHERE c.`card_no` = ? LIMIT 1",
    )
    .bind(card_no)
    .fetch_optional(pool)
    .await?;
    let Some((status, available)) = row else {
        return Ok((false, 0));
    };
    Ok((
        status == chargepilot_domain::card::CardStatus::Active,
        available.unwrap_or(0),
    ))
}
