//! 账单签发、用户已读与用户端读取（旧版 settlement/bill_repo.go + user_bill.go
//! 的单体对应物）。账单从计费回执与支付事实推导：缺口 = 回执缺口为 0 且实费
//! 超预付时的差额；签发幂等（订单唯一键 uk_order 复用），账单日期取充电开始
//! 时间而非计费执行时间。
//!
//! 生产接线：`issue_bill_in_tx` 在计费入账事务内调用（billing.rs 的 existing
//! 重放与正常入账两个成功路径），保证账单与费用回执同事务落地；`issue_bill`
//! 保留独立入口供补签与测试（自开 READ COMMITTED 事务）。

use chargepilot_domain::money::BillStatus;
use sqlx::{Acquire, MySqlConnection, MySqlPool};

/// 签发后的账单事实。
#[derive(Debug, Clone)]
pub struct BillRow {
    pub id: u64,
    pub bill_no: String,
    pub charge_order_id: u64,
    pub electric_cents: i64,
    pub service_cents: i64,
    pub total_cents: i64,
    pub prepaid_cents: i64,
    pub refund_cents: i64,
    pub shortfall_cents: i64,
    pub issued: bool,
}

/// 为已计费订单签发账单；计费未完成或实费为零返回 Ok(None)（旧版 Missing 语义）。
/// 幂等：同一订单重复签发复用既有账单。
pub async fn issue_bill(pool: &MySqlPool, charge_order_id: u64) -> anyhow::Result<Option<BillRow>> {
    // 项目事务惯例：先 SET 隔离级别再 begin（事务中 SET 会报错）。
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let row = issue_bill_in_tx(c, charge_order_id).await?;
    tx.commit().await?;
    Ok(row)
}

/// 在调用方事务内签发账单（生产接线入口：费用入账与账单同事务）。
/// 幂等：靠 uk_order(charge_order_id, created_month) 唯一键
/// `ON DUPLICATE KEY UPDATE id = LAST_INSERT_ID(id)` 复用既有账单；
/// 仅新插入时追加订单时间线事件（事件号由账单号派生，天然幂等）。
/// 读取（订单、回执、支付事实）全部走传入连接，不加新锁（计费事务已持有
/// 支付单 FOR UPDATE，此处只读不改变锁顺序）。
pub async fn issue_bill_in_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> anyhow::Result<Option<BillRow>> {
    // 订单（已结束）与计费回执。
    // 投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type BillOrderRow = (String, u64, Option<u64>, Option<String>, Option<String>);
    let order: Option<BillOrderRow> = sqlx::query_as(
        "SELECT `order_no`, `user_id`, `payment_order_id`, \
                DATE_FORMAT(`started_at`, '%Y-%m-%d %H:%i:%s.%f'), CAST(`charged_kwh` AS CHAR) \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((order_no, user_id, payment_order_id, started_at, _kwh)) = order else {
        return Ok(None);
    };
    let Some((electric, service, total, receipt_shortfall)): Option<(i64, i64, i64, i64)> =
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
        .await?
    else {
        return Ok(None);
    };
    if total <= 0 {
        return Ok(None);
    }
    // 支付事实（预付与已退；缺失按零预付推导缺口）。
    let (prepaid, refunded) = match payment_order_id {
        Some(pid) => {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT `paid_cents`, `refunded_cents` FROM `payment_order` WHERE `id` = ?",
            )
            .bind(pid as i64)
            .fetch_one(&mut *c)
            .await?
        }
        None => (0, 0),
    };
    let shortfall = if receipt_shortfall <= 0 && total > prepaid {
        total - prepaid
    } else {
        receipt_shortfall
    };
    let bill_no = format!("BILL{charge_order_id:020}");
    // 账单日期使用充电发生时间，不使用计费执行时间。
    let issued_at = match &started_at {
        Some(s) => s.clone(),
        None => chrono::Utc::now()
            .format("%Y-%m-%d %H:%i:%s.%f")
            .to_string(),
    };
    // 分区月份跟随账单日期（旧版 issuedAt 月首语义）：跨月重放命中同一唯一键，
    // 幂等复用而非重复签发。
    let issued_ts = chrono::NaiveDateTime::parse_from_str(&issued_at, "%Y-%m-%d %H:%i:%s.%f")
        .map(|naive| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    let month = chargepilot_mod_order::month_start_of(issued_ts);

    // 幂等插入：先按 uk_order 查既有账单复用（旧版 Count + Pluck 语义）；
    // 查无再纯 INSERT，并发撞唯一键时回读复用。事件只在真正新插入后追加
    // （event_id 由账单号派生，重放不重复）。
    let existing: Option<u64> = sqlx::query_scalar(
        "SELECT `id` FROM `charge_bill` WHERE `charge_order_id` = ? AND `created_month` = ?",
    )
    .bind(charge_order_id as i64)
    .bind(month)
    .fetch_optional(&mut *c)
    .await?;
    let bill_id = match existing {
        Some(id) => id,
        None => {
            let insert = sqlx::query(
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
            .bind(&bill_no)
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
            .bind(BillStatus::ISSUED)
            .bind(&issued_at)
            .bind(month)
            .execute(&mut *c)
            .await;
            // 并发窗口撞 uk_order（或 uk_bill_no）：账单已由并发方创建，回读复用，
            // 不再追加事件。
            let inserted_new = match &insert {
                Ok(_) => true,
                Err(e) => {
                    let dup = matches!(e, sqlx::Error::Database(db) if db.is_unique_violation());
                    if !dup {
                        return Err(anyhow::anyhow!(e.to_string()));
                    }
                    false
                }
            };
            let id: u64 = match insert {
                Ok(_) => {
                    sqlx::query_scalar("SELECT LAST_INSERT_ID()")
                        .fetch_one(&mut *c)
                        .await?
                }
                Err(_) => {
                    sqlx::query_scalar(
                        "SELECT `id` FROM `charge_bill` \
                     WHERE `charge_order_id` = ? AND `created_month` = ?",
                    )
                    .bind(charge_order_id as i64)
                    .bind(month)
                    .fetch_one(&mut *c)
                    .await?
                }
            };
            if inserted_new {
                // 新插入时追加账单事件到订单时间线（event_id 由账单号派生，重放不重复）。
                let event_id = format!("BILL-I{bill_no}");
                chargepilot_mod_order::append_event_in_tx(
                    c,
                    charge_order_id,
                    &event_id,
                    "bill_issued",
                    "billing",
                    &format!(
                        "bill={bill_no} total={total} prepaid={prepaid} shortfall={shortfall}"
                    ),
                    chrono::Utc::now().timestamp_micros(),
                )
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
            id
        }
    };
    let _ = order_no;
    Ok(Some(BillRow {
        id: bill_id,
        bill_no,
        charge_order_id,
        electric_cents: electric,
        service_cents: service,
        total_cents: total,
        prepaid_cents: prepaid,
        refund_cents: refunded,
        shortfall_cents: shortfall,
        issued: true,
    }))
}

// ---------------------------------------------------------------------------
// 用户端读取（旧版 BillRepo.LoadByID / ListBills + UserBillService 编排）。
// 响应字段对齐旧版 Bill 结构的 JSON 形状：id/user_id 为十进制字符串，
// charge_order_id 与 payment_order_id 为数字（payment 可空），
// 时间统一 `+00:00` 后缀，列表补查已读、详情不补（旧版 LoadByID 无已读补查）。
// ---------------------------------------------------------------------------

/// 账单行投影列清单（DECIMAL/ENUM/BIGINT UNSIGNED 读侧 CAST AS CHAR 铁律；
/// 时间统一 `+00:00` 输出）。列顺序与 `bill_row_value` 的下标一一对应。
const BILL_COLUMNS: &str = "CAST(b.`id` AS CHAR), \
     CAST(b.`bill_no` AS CHAR), \
     (SELECT c.`order_no` FROM `charge_order` c WHERE c.`id` = b.`charge_order_id` LIMIT 1), \
     CAST(b.`charge_order_id` AS CHAR), CAST(b.`payment_order_id` AS CHAR), \
     CAST(b.`user_id` AS CHAR), CAST(b.`device_id` AS CHAR), b.`port_no`, \
     b.`electric_cents`, b.`service_cents`, b.`total_cents`, b.`prepaid_cents`, \
     b.`refund_cents`, b.`shortfall_cents`, \
     CAST(b.`charged_kwh` AS CHAR), b.`charged_seconds`, CAST(b.`status` AS CHAR), \
     DATE_FORMAT(b.`issued_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00'), \
     DATE_FORMAT(b.`settled_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00')";

/// 行 → 响应对象（旧版 Bill 结构的 JSON 形状：id/user_id 十进制字符串，
/// charge_order_id/payment_order_id 数字，payment 可空；19 列超 sqlx 元组
/// FromRow 上限，逐列 try_get）。
fn bill_row_value(row: &sqlx::mysql::MySqlRow, read: bool) -> serde_json::Value {
    use sqlx::Row;
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

/// 按用户和账单 ID 查询详情（归属同时限定，越权与不存在同为 None → HTTP 404）。
pub async fn load_user_bill(
    pool: &MySqlPool,
    user_id: u64,
    bill_id: u64,
) -> anyhow::Result<Option<serde_json::Value>> {
    let sql = format!(
        "SELECT {BILL_COLUMNS} FROM `charge_bill` b \
         WHERE b.`id` = ? AND b.`user_id` = ? LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(bill_id as i64)
        .bind(user_id as i64)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| bill_row_value(&r, false)))
}

/// 按用户分页倒序读取账单并补充已读状态（旧版 ListBills：issued_at DESC, id DESC；
/// 已读查询失败时按未读展示——此处失败即整体失败，由 HTTP 503 兜底）。
pub async fn list_user_bills(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> anyhow::Result<(Vec<serde_json::Value>, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM `charge_bill` WHERE `user_id` = ?")
        .bind(user_id as i64)
        .fetch_one(pool)
        .await?;
    let sql = format!(
        "SELECT {BILL_COLUMNS} FROM `charge_bill` b \
         WHERE b.`user_id` = ? ORDER BY b.`issued_at` DESC, b.`id` DESC LIMIT ?, ?"
    );
    let rows = sqlx::query(&sql)
        .bind(user_id as i64)
        .bind((page.max(1) as i64 - 1) * page_size as i64)
        .bind(page_size as i64)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok((Vec::new(), total));
    }
    // 已读状态补查（旧版按用户 + 账单 ID 集合查 charge_bill_read）。
    // 每页上限 100 行，按主键前缀逐行点查代价可忽略；一行查询失败整体返回，
    // 由 HTTP 层按 503 兜底（旧版「已读查询失败按未读展示」的容错在用户可见
    // 语义上等价于未读，此处选择显式失败以暴露存储异常）。
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        use sqlx::Row;
        let bill_id: u64 = row
            .try_get::<String, _>(0)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let read: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM `charge_bill_read` WHERE `user_id` = ? AND `bill_id` = ? LIMIT 1",
        )
        .bind(user_id as i64)
        .bind(bill_id as i64)
        .fetch_optional(pool)
        .await?;
        items.push(bill_row_value(row, read.is_some()));
    }
    Ok((items, total))
}

/// 用户已读账单（旧版 user_bill.go 已读编排）：归属校验后幂等记录已读时刻。
/// 返回 Ok(false) 表示账单不存在或不属于该用户（HTTP 统一 404，不暴露存在性）。
pub async fn mark_bill_read(pool: &MySqlPool, user_id: u64, bill_id: u64) -> anyhow::Result<bool> {
    let month: Option<chrono::NaiveDate> = sqlx::query_scalar(
        "SELECT `created_month` FROM `charge_bill` WHERE `id` = ? AND `user_id` = ?",
    )
    .bind(bill_id as i64)
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await?;
    let Some(month) = month else {
        return Ok(false);
    };
    sqlx::query(
        "INSERT IGNORE INTO `charge_bill_read` (`bill_id`, `user_id`, `read_at`, `created_month`) \
         VALUES (?, ?, UTC_TIMESTAMP(3), ?)",
    )
    .bind(bill_id as i64)
    .bind(user_id as i64)
    .bind(month)
    .execute(pool)
    .await?;
    Ok(true)
}
