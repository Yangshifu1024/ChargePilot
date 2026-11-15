//! 后台用户充值支付页（旧版 wallet/recharge_history.go 与
//! payment/payment_view_repo.go 的 wallet_recharge 子集对齐）。
//! 只读：不把待支付单视为已到账，金额取已保存的支付与退款记录；
//! 用户存在性与权限由身份用例核对，本模块不提供资金写入。

use crate::{Page, WalletError};
use serde::Serialize;
use sqlx::MySqlPool;

/// 后台充值历史的一行（旧版 AdminRechargeRow 逐字段对齐；
/// user_id 沿用旧版 `,string` 契约按十进制字符串输出）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AdminRechargeRow {
    /// 支付主键，保持既有整数契约。
    pub payment_order_id: u64,
    /// 商户支付单号。
    pub order_no: String,
    /// 付款人，公开为十进制字符串。
    pub user_id: String,
    /// wallet_recharge 余额充值（本查询固定过滤）。
    pub biz_type: String,
    /// payment_order.pay_method：wechat、balance、mixed 或 coupon。
    pub pay_method: String,
    /// 支付单内部流程状态，保留关闭和失败等原因。
    pub status: String,
    /// 金额确认后的 pending/paid/refunded/partial_refunded（按到账与退款金额派生）。
    pub payment_status: String,
    /// 应付金额，分。
    pub total_cents: i64,
    /// 已确认到账金额，分。
    pub paid_cents: i64,
    /// 累计成功退款金额，分。
    pub refunded_cents: i64,
    /// 已确认到账时刻，未到账为空。
    pub paid_at: Option<String>,
    /// 支付创建时刻。
    pub created_at: String,
    /// 当前有效关联充电主键，充值或无关联为空。
    pub charge_order_id: Option<u64>,
    /// 当前有效关联充电单号，充值或无关联为空。
    pub charge_order_no: Option<String>,
}

/// 分页读取 userID 的充值支付（旧版 WalletRecharges → PaymentOrders 子集）。
/// 标量子查询保持一条，历史异常关联也不会放大分页总数；充值单始终无关联充电单。
pub async fn admin_recharges(
    pool: &MySqlPool,
    user_id: u64,
    page: u32,
    page_size: u32,
) -> Result<Page<AdminRechargeRow>, WalletError> {
    const PAYMENT_STATUS_SQL: &str = "CASE WHEN p.`paid_cents` > 0 AND p.`refunded_cents` >= p.`paid_cents` THEN 'refunded' \
      WHEN p.`refunded_cents` > 0 THEN 'partial_refunded' \
      WHEN p.`paid_cents` > 0 OR p.`status` IN ('paid','partial_refunded','refunded') THEN 'paid' \
      ELSE 'pending' END";
    // 标量子查询限制为一条（旧版 linked 定义），与充电支付列表同一形状。
    const LINKED_FROM: &str = " FROM `charge_order` c WHERE p.`biz_type` = 'charge' \
      AND c.`payment_order_id` = p.`id` AND c.`user_id` = p.`user_id` \
      AND c.`deleted_at` IS NULL ORDER BY c.`created_at` DESC, c.`id` DESC LIMIT 1";
    let list_sql = format!(
        "SELECT p.`id` AS `payment_order_id`, CAST(p.`order_no` AS CHAR) AS `order_no`, \
                CAST(p.`user_id` AS CHAR) AS `user_id`, CAST(p.`biz_type` AS CHAR) AS `biz_type`, \
                CAST(p.`pay_method` AS CHAR) AS `pay_method`, CAST(p.`status` AS CHAR) AS `status`, \
                {PAYMENT_STATUS_SQL} AS `payment_status`, \
                p.`total_cents`, p.`paid_cents`, p.`refunded_cents`, \
                DATE_FORMAT(p.`paid_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `paid_at`, \
                DATE_FORMAT(p.`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                (SELECT c.`id`{LINKED_FROM}) AS `charge_order_id`, \
                (SELECT CAST(c.`order_no` AS CHAR){LINKED_FROM}) AS `charge_order_no` \
         FROM `payment_order` p \
         WHERE p.`deleted_at` IS NULL AND p.`user_id` = ? AND p.`biz_type` = 'wallet_recharge' \
         ORDER BY p.`created_at` DESC, p.`id` DESC LIMIT ? OFFSET ?"
    );
    let offset = ((page.max(1) - 1) * page_size.max(1)) as i64;
    let items = sqlx::query_as::<_, AdminRechargeRow>(&list_sql)
        .bind(user_id as i64)
        .bind(page_size.max(1))
        .bind(offset)
        .fetch_all(pool)
        .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM `payment_order` p \
         WHERE p.`deleted_at` IS NULL AND p.`user_id` = ? AND p.`biz_type` = 'wallet_recharge'",
    )
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
