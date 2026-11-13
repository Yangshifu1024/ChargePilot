//! 账单签发与用户已读（旧版 settlement/bill_repo.go + user_bill.go 的单体对应物）。
//! 账单从计费回执与支付事实推导：缺口 = 回执缺口为 0 且实费超预付时的差额；
//! 签发幂等（订单唯一键复用），账单日期取充电开始时间而非计费执行时间。

use chargepilot_domain::money::BillStatus;
use sqlx::{Acquire, MySqlPool};

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
    // 订单（已结束）与计费回执。
    let order: Option<(String, u64, Option<u64>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT `order_no`, `user_id`, `payment_order_id`, \
                DATE_FORMAT(`started_at`, '%Y-%m-%d %H:%i:%s.%f'), CAST(`charged_kwh` AS CHAR) \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
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
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    if total <= 0 {
        return Ok(None);
    }
    // 支付事实（预付与已退；缺失按零预付推导缺口）。
    let (prepaid, refunded) = match payment_order_id {
        Some(pid) => sqlx::query_as::<_, (i64, i64)>(
            "SELECT `paid_cents`, `refunded_cents` FROM `payment_order` WHERE `id` = ?",
        )
        .bind(pid as i64)
        .fetch_one(pool)
        .await?,
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
        None => chrono::Utc::now().format("%Y-%m-%d %H:%i:%s.%f").to_string(),
    };
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());

    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 幂等：已有账单直接复用（旧版 Count + Pluck 语义）。
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
            .await?;
            let id: u64 = sqlx::query_scalar("SELECT LAST_INSERT_ID()")
                .fetch_one(&mut *c)
                .await?;
            // 账单事件追加到订单时间线（event_id 由账单号派生，天然幂等）。
            let event_id = format!("BILL-I{bill_no}");
            chargepilot_mod_order::append_event_in_tx(
                &mut *c,
                charge_order_id,
                &event_id,
                "bill_issued",
                "billing",
                &format!("bill={bill_no} total={total} prepaid={prepaid} shortfall={shortfall}"),
                chrono::Utc::now().timestamp_micros(),
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let _ = order_no;
            id
        }
    };
    tx.commit().await?;
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

/// 用户已读账单（旧版 user_bill.go 已读编排）：归属校验后幂等记录已读时刻。
pub async fn mark_bill_read(
    pool: &MySqlPool,
    user_id: u64,
    bill_id: u64,
) -> anyhow::Result<bool> {
    let owner: Option<u64> = sqlx::query_scalar(
        "SELECT `user_id` FROM `charge_bill` WHERE `id` = ?",
    )
    .bind(bill_id as i64)
    .fetch_optional(pool)
    .await?;
    let Some(owner) = owner else {
        return Ok(false);
    };
    if owner != user_id {
        anyhow::bail!("账单不属于该用户");
    }
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
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
