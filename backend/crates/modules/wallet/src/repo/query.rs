//! C 端钱包查询组合：余额、流水、充值付款条件与退款进度。
//! 只查询调用方用户的数据；未开户返回可用的零余额视图，不以流水重算余额。

use crate::{Page, WalletError, account};
use chargepilot_domain::money::PaymentOrderStatus;
use serde::Serialize;
use sqlx::MySqlPool;

/// 余额视图（含可用额派生）。
#[derive(Debug, Clone, Serialize)]
pub struct BalanceView {
    pub balance_cents: i64,
    pub frozen_cents: i64,
    pub available_cents: i64,
    /// 未开户显示 active。
    pub status: String,
}

/// 读取 userID 的余额；未开户返回可用的零余额视图。
pub async fn balance(pool: &MySqlPool, user_id: u64) -> Result<BalanceView, WalletError> {
    match account::find_by_user(pool, user_id).await? {
        None => Ok(BalanceView {
            balance_cents: 0,
            frozen_cents: 0,
            available_cents: 0,
            status: "active".into(),
        }),
        Some(v) => {
            let available = (v.balance_cents - v.frozen_cents).max(0);
            Ok(BalanceView {
                balance_cents: v.balance_cents,
                frozen_cents: v.frozen_cents,
                available_cents: available,
                status: v.status,
            })
        }
    }
}

/// 流水行（API JSON 序列化形态）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TxnRow {
    pub txn_no: String,
    pub direction: String,
    pub amount_cents: i64,
    pub balance_after_cents: i64,
    pub biz_type: String,
    pub biz_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: Option<String>,
}

/// 按用户、分页与 direction 查询流水；空方向查全部。
pub async fn transactions(
    pool: &MySqlPool,
    user_id: u64,
    direction: Option<&str>,
    page: u32,
    page_size: u32,
) -> Result<Page<TxnRow>, WalletError> {
    if let Some(d) = direction
        && d != "in"
        && d != "out"
    {
        return Err(WalletError::RequestConflict);
    }
    let offset = ((page.max(1) - 1) * page_size.max(1)) as i64;
    let dir_clause = match direction {
        Some("in") => " AND `direction` = 'in'",
        Some("out") => " AND `direction` = 'out'",
        _ => "",
    };
    let list_sql = format!(
        "SELECT `txn_no`, `direction`, `amount_cents`, `balance_after_cents`, `biz_type`, \
                `biz_ref`, `note`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at` \
         FROM `wallet_txn` WHERE `user_id` = ?{dir_clause} \
         ORDER BY `id` DESC LIMIT ? OFFSET ?"
    );
    let items = sqlx::query_as::<_, TxnRow>(&list_sql)
        .bind(user_id as i64)
        .bind(page_size.max(1))
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let count_sql = format!("SELECT COUNT(*) FROM `wallet_txn` WHERE `user_id` = ?{dir_clause}");
    let (total,): (i64,) = sqlx::query_as(&count_sql)
        .bind(user_id as i64)
        .fetch_one(pool)
        .await?;
    Ok(Page {
        items,
        total,
        page: page.max(1),
        page_size: page_size.max(1),
    })
}

/// 充值申请行：联表带出支付状态与有效期。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RechargeRow {
    pub request_id: String,
    pub amount_cents: i64,
    pub status: PaymentOrderStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: Option<String>,
    /// 未支付且未过期才允许继续付款（now 判定后回填）。
    pub can_pay: bool,
}

/// 分页读取 userID 的充值申请及支付状态，按创建顺序倒序。
pub async fn recharges(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Page<RechargeRow>, WalletError> {
    let offset = ((page.max(1) - 1) * page_size.max(1)) as i64;
    let items = sqlx::query_as::<_, RechargeRow>(
        "SELECT CAST(r.`request_id` AS CHAR), r.`amount_cents`, p.`status`, \
                DATE_FORMAT(p.`expired_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `expires_at`, \
                DATE_FORMAT(r.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                FALSE AS `can_pay` \
         FROM `wallet_recharge_request` r \
         JOIN `payment_order` p ON p.`id` = r.`payment_order_id` \
         WHERE r.`user_id` = ? \
         ORDER BY r.`created_at` DESC, r.`request_id` LIMIT ? OFFSET ?",
    )
    .bind(user_id as i64)
    .bind(page_size.max(1))
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM `wallet_recharge_request` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(pool)
            .await?;
    let mut items = items;
    for r in items.iter_mut() {
        r.can_pay = r.status == PaymentOrderStatus::Initiated
            && r.expires_at
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|t| t.with_timezone(&chrono::Utc) > now);
    }
    Ok(Page {
        items,
        total,
        page: page.max(1),
        page_size: page_size.max(1),
    })
}

/// 退款分笔行（关联 refund_record）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RefundPartRow {
    pub refund_no: String,
    pub refund_cents: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// 频次风控人工审核展示字段。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RefundReviewView {
    pub approved: bool,
    pub comment: String,
    pub actor_id: u64,
}

/// 一笔申请及原路退款的进度；status 为派生展示状态，非库表枚举。
#[derive(Debug, Clone, Serialize)]
pub struct RefundView {
    pub request_id: String,
    pub amount_cents: i64,
    pub refunded_cents: i64,
    /// pending / processing / needs_review / success / manual_review / rejected。
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub refund_orders: Vec<RefundPartRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<RefundReviewView>,
}

/// 分页读取 userID 的退款申请，一次批量补齐本页退款、审核及冻结链接。
/// 每次查询至多四条 SQL，不随本页记录数增长，不执行资金写入。
pub async fn refunds(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> Result<Page<RefundView>, WalletError> {
    let offset = ((page.max(1) - 1) * page_size.max(1)) as i64;
    let requests = sqlx::query_as::<_, (String, i64, Option<String>, Option<String>)>(
        "SELECT CAST(`request_id` AS CHAR), `amount_cents`, CAST(`reason` AS CHAR), \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') \
         FROM `wallet_refund_request` WHERE `user_id` = ? \
         ORDER BY `created_at` DESC, `request_id` LIMIT ? OFFSET ?",
    )
    .bind(user_id as i64)
    .bind(page_size.max(1))
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM `wallet_refund_request` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(pool)
            .await?;
    let ids: Vec<String> = requests.iter().map(|r| r.0.clone()).collect();
    if ids.is_empty() {
        return Ok(Page {
            items: vec![],
            total,
            page: page.max(1),
            page_size: page_size.max(1),
        });
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    // 退款分笔（联 refund_record 取状态与失败原因）。
    let parts_sql = format!(
        "SELECT CAST(p.`request_id` AS CHAR), rr.`refund_no`, p.`amount_cents` AS `refund_cents`, \
                rr.`status`, rr.`failure_reason`, \
                DATE_FORMAT(rr.`completed_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `completed_at` \
         FROM `wallet_refund_part` p JOIN `refund_record` rr ON rr.`id` = p.`refund_record_id` \
         WHERE p.`request_id` IN ({placeholders}) ORDER BY rr.`id`"
    );
    let mut parts_q = sqlx::query_as::<
        _,
        (String, String, i64, String, Option<String>, Option<String>),
    >(&parts_sql);
    for id in &ids {
        parts_q = parts_q.bind(id);
    }
    let part_rows = parts_q.fetch_all(pool).await?;
    // 审核（每申请至多一条）与冻结链接计数。
    let review_sql = format!(
        "SELECT CAST(`request_id` AS CHAR), `approved`, `comment`, `actor_id` FROM `wallet_risk_review` \
         WHERE `request_id` IN ({placeholders})"
    );
    let mut review_q = sqlx::query_as::<_, (String, bool, String, u64)>(&review_sql);
    for id in &ids {
        review_q = review_q.bind(id);
    }
    let review_rows: std::collections::HashMap<String, RefundReviewView> = review_q
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(rid, approved, comment, actor_id)| {
            (
                rid,
                RefundReviewView {
                    approved,
                    comment,
                    actor_id,
                },
            )
        })
        .collect();
    let link_sql = format!(
        "SELECT CAST(`request_id` AS CHAR), COUNT(*) FROM `wallet_risk_freeze_link` \
         WHERE `request_id` IN ({placeholders}) GROUP BY `request_id`"
    );
    let mut link_q = sqlx::query_as::<_, (String, i64)>(&link_sql);
    for id in &ids {
        link_q = link_q.bind(id);
    }
    let link_rows: std::collections::HashMap<String, i64> =
        link_q.fetch_all(pool).await?.into_iter().collect();
    let mut items = Vec::with_capacity(requests.len());
    for (request_id, amount_cents, reason, created_at) in requests {
        let parts: Vec<RefundPartRow> = part_rows
            .iter()
            .filter(|p| p.0 == request_id)
            .map(|p| RefundPartRow {
                refund_no: p.1.clone(),
                refund_cents: p.2,
                status: p.3.clone(),
                failure_reason: p.4.clone(),
                completed_at: p.5.clone(),
            })
            .collect();
        let review = review_rows.get(&request_id).cloned();
        let linked = link_rows.get(&request_id).copied().unwrap_or(0);
        items.push(compose_refund_view(
            &request_id,
            amount_cents,
            reason,
            created_at,
            parts,
            review,
            linked,
        ));
    }
    Ok(Page {
        items,
        total,
        page: page.max(1),
        page_size: page_size.max(1),
    })
}

/// 由分笔状态、成功金额与风控链接组合退款进度视图。
/// 退款成功以渠道确认金额为准；未审核且有关联冻结时显示待人工审核，明确拒绝后才显示 rejected。
#[allow(clippy::too_many_arguments)]
fn compose_refund_view(
    request_id: &str,
    amount_cents: i64,
    reason: Option<String>,
    created_at: Option<String>,
    parts: Vec<RefundPartRow>,
    review: Option<RefundReviewView>,
    linked: i64,
) -> RefundView {
    let mut view = RefundView {
        request_id: request_id.to_string(),
        amount_cents,
        refunded_cents: 0,
        status: "pending".into(),
        reason,
        created_at,
        refund_orders: parts,
        review,
    };
    for part in &view.refund_orders {
        match part.status.as_str() {
            "success" => view.refunded_cents += part.refund_cents,
            // failed 或 needs_review（部分渠道终态存疑）→ 需要复核展示态。
            "failed" | "needs_review" => view.status = "needs_review".into(),
            "processing" if view.status != "needs_review" => {
                view.status = "processing".into();
            }
            _ => {}
        }
    }
    if view.refunded_cents == view.amount_cents {
        view.status = "success".into();
    }
    if linked > 0 && view.review.is_none() {
        view.status = "manual_review".into();
    } else if view.review.as_ref().is_some_and(|r| !r.approved) {
        view.status = "rejected".into();
    }
    view
}
