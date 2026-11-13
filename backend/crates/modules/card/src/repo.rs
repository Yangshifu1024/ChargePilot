//! 刷卡相关表的访问层（旧版 card_schema.go + card_repo.go 的读取部分）。
//! 写入与资金由 swipe 用例在事务内编排，本模块只提供行模型与共享读取。

use sqlx::{MySqlPool, Row};

/// 在线卡行（online_card）。
#[derive(Debug, Clone)]
pub struct OnlineCardRow {
    pub id: u64,
    pub card_no: String,
    pub user_id: u64,
    pub status: String,
}

/// 刷卡会话行（card_charge）：占用、累计购买与首购冻结快照。
#[derive(Debug, Clone)]
pub struct CardChargeRow {
    pub charge_order_id: u64,
    pub card_id: u64,
    pub port_code: String,
    pub active_port: Option<String>,
    pub paid_cents: i64,
    pub purchased_minutes: u16,
    pub max_minutes: u16,
    pub card_no: String,
    pub wallet_after_cents: i64,
    pub package_json: String,
}

/// 刷卡操作行（card_operation）：一次首购或续购的幂等结果。
#[derive(Debug, Clone)]
pub struct CardOperationRow {
    pub operation_id: String,
    pub device_id: String,
    pub event_id: String,
    pub charge_order_id: u64,
    pub card_id: u64,
    pub kind: String,
    pub price_cents: i64,
    pub minutes: u16,
    pub status: String,
}

/// 按卡号读在线卡；`for_update` 为真时加排他行锁（刷卡扣款路径）。
pub async fn find_online_card(
    conn: &mut sqlx::MySqlConnection,
    card_no: &str,
    for_update: bool,
) -> Result<Option<OnlineCardRow>, sqlx::Error> {
    let lock = if for_update { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT `id`, CAST(`card_no` AS CHAR) AS `card_no`, `user_id`, `status` \
         FROM `online_card` WHERE `card_no` = ?{lock}"
    );
    let row = sqlx::query(&sql).bind(card_no).fetch_optional(conn).await?;
    Ok(row.map(|r| OnlineCardRow {
        id: r.get::<u64, _>("id"),
        card_no: r.get("card_no"),
        user_id: r.get::<u64, _>("user_id"),
        status: r.get("status"),
    }))
}

/// 按订单读刷卡会话（判断订单是否刷卡单、累计时长与冻结套餐）。
pub async fn find_card_charge(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<CardChargeRow>, sqlx::Error> {
    let row: Option<(
        u64,
        u64,
        String,
        Option<String>,
        i64,
        u16,
        u16,
        String,
        i64,
        serde_json::Value,
    )> = sqlx::query_as(
        "SELECT `charge_order_id`, `card_id`, `port_code`, `active_port`, `paid_cents`, \
                    `purchased_minutes`, `max_minutes`, CAST(`card_no` AS CHAR) AS `card_no`, \
                    `wallet_after_cents`, CAST(`package_json` AS JSON) \
             FROM `card_charge` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(
            charge_order_id,
            card_id,
            port_code,
            active_port,
            paid_cents,
            purchased_minutes,
            max_minutes,
            card_no,
            wallet_after_cents,
            package_json,
        )| {
            CardChargeRow {
                charge_order_id,
                card_id,
                port_code,
                active_port,
                paid_cents,
                purchased_minutes,
                max_minutes,
                card_no,
                wallet_after_cents,
                package_json: package_json.to_string(),
            }
        },
    ))
}

/// 统计端口当前占用的刷卡会话数（旧版 ActiveSessionCount）。
pub async fn active_session_count(pool: &MySqlPool, port_code: &str) -> Result<i64, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `card_charge` WHERE `active_port` = ?")
            .bind(port_code)
            .fetch_one(pool)
            .await?;
    Ok(count)
}
