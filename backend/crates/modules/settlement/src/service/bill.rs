//! 账单签发、用户已读与用户端读取。
//! 账单从计费回执与支付事实推导：缺口 = 回执缺口为 0 且实费
//! 超预付时的差额；签发幂等（订单唯一键 uk_order 复用），账单日期取充电开始
//! 时间而非计费执行时间。SQL 在 repo/bill.rs。
//!
//! 生产接线：`issue_bill_in_tx` 在计费入账事务内调用（billing.rs 的 existing
//! 重放与正常入账两个成功路径），保证账单与费用回执同事务落地；`issue_bill`
//! 保留独立入口供补签与测试（自开 READ COMMITTED 事务）。

use crate::repo::{bill, set_read_committed};
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

/// 为已计费订单签发账单；计费未完成或实费为零返回 Ok(None)。
/// 幂等：同一订单重复签发复用既有账单。
pub async fn issue_bill(pool: &MySqlPool, charge_order_id: u64) -> anyhow::Result<Option<BillRow>> {
    // 项目事务惯例：先 SET 隔离级别再 begin（事务中 SET 会报错）。
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let row = issue_bill_in_tx(c, charge_order_id).await?;
    tx.commit().await?;
    Ok(row)
}

/// 在调用方事务内签发账单（生产接线入口：费用入账与账单同事务）。
/// 幂等：靠 uk_order(charge_order_id, created_month) 唯一键复用既有账单；
/// 仅新插入时追加订单时间线事件（事件号由账单号派生，天然幂等）。
/// 读取（订单、回执、支付事实）全部走传入连接，不加新锁（计费事务已持有
/// 支付单 FOR UPDATE，此处只读不改变锁顺序）。
#[allow(clippy::too_many_lines)]
pub async fn issue_bill_in_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> anyhow::Result<Option<BillRow>> {
    let Some((order_no, user_id, payment_order_id, started_at, _kwh)) =
        bill::bill_order_tx(c, charge_order_id).await?
    else {
        return Ok(None);
    };
    let Some((electric, service, total, receipt_shortfall)) =
        bill::fee_receipt_tx(c, charge_order_id).await?
    else {
        return Ok(None);
    };
    if total <= 0 {
        return Ok(None);
    }
    // 支付事实（预付与已退；缺失按零预付推导缺口）。
    let (prepaid, refunded) = match payment_order_id {
        Some(pid) => bill::payment_paid_refunded_tx(c, pid).await?,
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
    // 分区月份跟随账单日期：跨月重放命中同一唯一键，
    // 幂等复用而非重复签发。
    let issued_ts = chrono::NaiveDateTime::parse_from_str(&issued_at, "%Y-%m-%d %H:%i:%s.%f")
        .map(|naive| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    let month = chargepilot_mod_order::month_start_of(issued_ts);

    // 幂等插入：先按 uk_order 查既有账单复用；
    // 查无再纯 INSERT，并发撞唯一键时回读复用。事件只在真正新插入后追加
    // （event_id 由账单号派生，重放不重复）。
    let existing = bill::find_bill_id_tx(c, charge_order_id, month).await?;
    let bill_id = match existing {
        Some(id) => id,
        None => {
            let insert = bill::insert_bill_tx(
                c,
                &bill_no,
                charge_order_id,
                payment_order_id,
                user_id,
                electric,
                service,
                total,
                prepaid,
                refunded,
                shortfall,
                &issued_at,
                month,
            )
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
                Ok(_) => bill::last_insert_id_tx(c).await?,
                Err(_) => bill::find_bill_id_tx(c, charge_order_id, month)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("账单撞键后回读缺失"))?,
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
// 用户端读取。
// 响应字段：id/user_id 为十进制字符串，
// charge_order_id 与 payment_order_id 为数字（payment 可空），
// 时间统一 `+00:00` 后缀，列表补查已读、详情不补。
// ---------------------------------------------------------------------------

/// 按用户和账单 ID 查询详情。
pub async fn load_user_bill(
    pool: &MySqlPool,
    user_id: u64,
    bill_id: u64,
) -> anyhow::Result<Option<serde_json::Value>> {
    Ok(bill::load_user_bill_row(pool, user_id, bill_id)
        .await?
        .map(|r| bill::bill_row_value(&r, false)))
}

/// 按用户分页倒序读取账单并补充已读状态（issued_at DESC, id DESC）；
/// 已读批量查询失败则整体失败，由 HTTP 503 兜底。
pub async fn list_user_bills(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> anyhow::Result<(Vec<serde_json::Value>, i64)> {
    let total = bill::bill_count(pool, user_id).await?;
    let rows = bill::bill_page_rows(
        pool,
        user_id,
        (page.max(1) as i64 - 1) * page_size as i64,
        page_size as i64,
    )
    .await?;
    if rows.is_empty() {
        return Ok((Vec::new(), total));
    }
    // 本页账单 ID 批量查 charge_bill_read（单条 IN 查询）；失败整体返回 503。
    let bill_ids: Vec<u64> = rows
        .iter()
        .map(|row| {
            use sqlx::Row;
            row.try_get::<String, _>(0)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
        .collect();
    let read_flags = bill::bill_read_flags(pool, user_id, &bill_ids).await?;
    let items = rows
        .iter()
        .map(|row| {
            use sqlx::Row;
            let bill_id: u64 = row
                .try_get::<String, _>(0)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            bill::bill_row_value(row, read_flags.contains(&bill_id))
        })
        .collect();
    Ok((items, total))
}

/// 用户已读账单：归属校验后幂等记录已读时刻。
/// 返回 Ok(false) 表示账单不存在或不属于该用户（HTTP 统一 404，不暴露存在性）。
pub async fn mark_bill_read(pool: &MySqlPool, user_id: u64, bill_id: u64) -> anyhow::Result<bool> {
    let Some(month) = bill::bill_month(pool, user_id, bill_id).await? else {
        return Ok(false);
    };
    bill::insert_bill_read_tx(pool, bill_id, user_id, month).await?;
    Ok(true)
}
