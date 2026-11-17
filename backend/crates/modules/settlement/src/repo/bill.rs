//! 账单签发与用户端读取的表访问：charge_bill / charge_fee_receipt /
//! charge_order / payment_order / charge_bill_read（蓝图第五章 repo 层）。

use chargepilot_domain::money::BillStatus;
use sqlx::{MySqlConnection, MySqlPool, Row};

/// 账单行投影列清单（DECIMAL/ENUM/BIGINT UNSIGNED 读侧 CAST AS CHAR 铁律；
/// 时间统一 `+00:00` 输出）。列顺序与 `bill_row_value` 的下标一一对应。
pub const BILL_COLUMNS: &str = "CAST(b.`id` AS CHAR), \
     CAST(b.`bill_no` AS CHAR), \
     (SELECT c.`order_no` FROM `charge_order` c WHERE c.`id` = b.`charge_order_id` LIMIT 1), \
     CAST(b.`charge_order_id` AS CHAR), CAST(b.`payment_order_id` AS CHAR), \
     CAST(b.`user_id` AS CHAR), CAST(b.`device_id` AS CHAR), b.`port_no`, \
     b.`electric_cents`, b.`service_cents`, b.`total_cents`, b.`prepaid_cents`, \
     b.`refund_cents`, b.`shortfall_cents`, \
     CAST(b.`charged_kwh` AS CHAR), b.`charged_seconds`, CAST(b.`status` AS CHAR), \
     DATE_FORMAT(b.`issued_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
     DATE_FORMAT(b.`settled_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00')";

/// 签发读取的订单投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type BillOrderRow = (String, u64, Option<u64>, Option<String>, Option<String>);

/// 订单（已结束）与签发所需基础事实。
pub async fn bill_order_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<BillOrderRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `order_no`, `user_id`, `payment_order_id`, \
                DATE_FORMAT(`started_at`, '%Y-%m-%d %H:%i:%s.%f'), CAST(`charged_kwh` AS CHAR) \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 计费回执费用（JSON 提取 + 缺口原值）。
pub async fn fee_receipt_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<(i64, i64, i64, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT \
         CAST(JSON_UNQUOTE(JSON_EXTRACT(`result_json`, '$.electric_cents')) AS SIGNED), \
         CAST(JSON_UNQUOTE(JSON_EXTRACT(`result_json`, '$.service_cents')) AS SIGNED), \
         CAST(JSON_UNQUOTE(JSON_EXTRACT(`result_json`, '$.total_cents')) AS SIGNED), \
         `shortfall_cents` \
         FROM `charge_fee_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 支付事实（预付与已退）。
pub async fn payment_paid_refunded_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as("SELECT `paid_cents`, `refunded_cents` FROM `payment_order` WHERE `id` = ?")
        .bind(payment_order_id as i64)
        .fetch_one(&mut *c)
        .await
}

/// 既有账单复用查询（uk_order：订单 + 分区月份）。
pub async fn find_bill_id_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    month: chrono::NaiveDate,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `id` FROM `charge_bill` WHERE `charge_order_id` = ? AND `created_month` = ?",
    )
    .bind(charge_order_id as i64)
    .bind(month)
    .fetch_optional(&mut *c)
    .await
}

/// 幂等插入账单（uk_order/uk_bill_no 冲突原样上抛，由调用方判定复用回读）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_bill_tx(
    c: &mut MySqlConnection,
    bill_no: &str,
    charge_order_id: u64,
    payment_order_id: Option<u64>,
    user_id: u64,
    electric: i64,
    service: i64,
    total: i64,
    prepaid: i64,
    refunded: i64,
    shortfall: i64,
    issued_at: &str,
    month: chrono::NaiveDate,
) -> Result<sqlx::mysql::MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_bill` (`bill_no`, `charge_order_id`, `payment_order_id`, \
         `user_id`, `device_id`, `port_no`, `electric_cents`, `service_cents`, \
         `total_cents`, `prepaid_cents`, `refund_cents`, `shortfall_cents`, \
         `charged_kwh`, `charged_seconds`, `status`, `issued_at`, `created_month`) \
         VALUES (?, ?, ?, ?, \
         (SELECT `device_id` FROM `charge_order` WHERE `id` = ?), \
         (SELECT `port_no` FROM `charge_order` WHERE `id` = ?), \
         ?, ?, ?, ?, ?, ?, \
         (SELECT `charged_kwh` FROM `charge_order` WHERE `id` = ?), \
         (SELECT `charged_seconds` FROM `charge_order` WHERE `id` = ?), \
         ?, ?, ?)",
    )
    .bind(bill_no)
    .bind(charge_order_id as i64)
    .bind(payment_order_id.map(|p| p as i64))
    .bind(user_id as i64)
    .bind(charge_order_id as i64)
    .bind(charge_order_id as i64)
    .bind(electric)
    .bind(service)
    .bind(total)
    .bind(prepaid)
    .bind(refunded)
    .bind(shortfall)
    .bind(charge_order_id as i64)
    .bind(charge_order_id as i64)
    .bind(BillStatus::Issued)
    .bind(issued_at)
    .bind(month)
    .execute(&mut *c)
    .await
}

/// 当前连接的 LAST_INSERT_ID（撞键回读前的正常取号）。
pub async fn last_insert_id_tx(c: &mut MySqlConnection) -> Result<u64, sqlx::Error> {
    sqlx::query_scalar("SELECT LAST_INSERT_ID()")
        .fetch_one(&mut *c)
        .await
}

/// 行 → 响应对象（Bill 结构的 JSON 形状：id/user_id 十进制字符串，
/// charge_order_id/payment_order_id 数字，payment 可空；19 列超 sqlx 元组
/// FromRow 上限，逐列 try_get）。
pub fn bill_row_value(row: &sqlx::mysql::MySqlRow, read: bool) -> serde_json::Value {
    let parse_u64 = |s: String| s.parse::<u64>().unwrap_or(0);
    let s = |i: usize| row.try_get::<String, _>(i).unwrap_or_default();
    let opt_s = |i: usize| row.try_get::<Option<String>, _>(i).unwrap_or(None);
    let num = |i: usize| row.try_get::<i64, _>(i).unwrap_or(0);
    let opt_num = |i: usize| row.try_get::<Option<i64>, _>(i).unwrap_or(None);
    serde_json::json!({
        "id": s(0),
        "bill_no": s(1),
        "order_no": opt_s(2).unwrap_or_default(),
        "charge_order_id": parse_u64(s(3)),
        "payment_order_id": opt_s(4).map(parse_u64),
        "user_id": s(5),
        "device_id": s(6),
        "port_no": num(7),
        "electric_cents": num(8),
        "service_cents": num(9),
        "total_cents": num(10),
        "prepaid_cents": num(11),
        "refund_cents": num(12),
        "shortfall_cents": num(13),
        "charged_kwh": opt_s(14),
        "charged_seconds": opt_num(15),
        "status": s(16),
        "issued_at": s(17),
        "settled_at": opt_s(18),
        "read": read,
    })
}

/// 按用户和账单 ID 查详情行（归属同时限定，越权与不存在同为 None → HTTP 404）。
pub async fn load_user_bill_row(
    pool: &MySqlPool,
    user_id: u64,
    bill_id: u64,
) -> Result<Option<sqlx::mysql::MySqlRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {BILL_COLUMNS} FROM `charge_bill` b \
         WHERE b.`id` = ? AND b.`user_id` = ? LIMIT 1"
    );
    sqlx::query(&sql)
        .bind(bill_id as i64)
        .bind(user_id as i64)
        .fetch_optional(pool)
        .await
}

/// 用户账单计数。
pub async fn bill_count(pool: &MySqlPool, user_id: u64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM `charge_bill` WHERE `user_id` = ?")
        .bind(user_id as i64)
        .fetch_one(pool)
        .await
}

/// 用户账单分页行（issued_at DESC, id DESC）。
pub async fn bill_page_rows(
    pool: &MySqlPool,
    user_id: u64,
    offset: i64,
    page_size: i64,
) -> Result<Vec<sqlx::mysql::MySqlRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {BILL_COLUMNS} FROM `charge_bill` b \
         WHERE b.`user_id` = ? ORDER BY b.`issued_at` DESC, b.`id` DESC LIMIT ?, ?"
    );
    sqlx::query(&sql)
        .bind(user_id as i64)
        .bind(offset)
        .bind(page_size)
        .fetch_all(pool)
        .await
}

/// 归属校验：取账单分区月份（不存在或不属于该用户同为 None）。
pub async fn bill_month(
    pool: &MySqlPool,
    user_id: u64,
    bill_id: u64,
) -> Result<Option<chrono::NaiveDate>, sqlx::Error> {
    sqlx::query_scalar("SELECT `created_month` FROM `charge_bill` WHERE `id` = ? AND `user_id` = ?")
        .bind(bill_id as i64)
        .bind(user_id as i64)
        .fetch_optional(pool)
        .await
}

/// 幂等记录已读时刻。
pub async fn insert_bill_read_tx(
    pool: &MySqlPool,
    bill_id: u64,
    user_id: u64,
    month: chrono::NaiveDate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT IGNORE INTO `charge_bill_read` (`bill_id`, `user_id`, `read_at`, `created_month`) \
         VALUES (?, ?, UTC_TIMESTAMP(3), ?)",
    )
    .bind(bill_id as i64)
    .bind(user_id as i64)
    .bind(month)
    .execute(pool)
    .await?;
    Ok(())
}

/// 批量已读标记（用户 + 本页账单 ID 集合）：一条 IN 查询替代逐行点查。
pub async fn bill_read_flags(
    pool: &MySqlPool,
    user_id: u64,
    bill_ids: &[u64],
) -> Result<std::collections::HashSet<u64>, sqlx::Error> {
    if bill_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let placeholders = vec!["?"; bill_ids.len()].join(", ");
    let sql = format!(
        "SELECT CAST(`bill_id` AS CHAR) FROM `charge_bill_read` \
         WHERE `user_id` = ? AND `bill_id` IN ({placeholders})"
    );
    let mut query = sqlx::query_scalar::<_, String>(&sql).bind(user_id as i64);
    for id in bill_ids {
        query = query.bind(*id as i64);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(rows.into_iter().filter_map(|s| s.parse().ok()).collect())
}
