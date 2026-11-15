//! feedback 表访问与读模型（蓝图第五章 repo 层）：
//! 后台分页列表读侧 + 回复/关闭/提交事务内的存储原语。

use crate::error::FeedbackError;
use chargepilot_domain::order::{ChargeOrderStatus, FeedbackStatus};
use serde_json::{Value, json};
use sqlx::{MySqlConnection, MySqlPool};

/// 后台分页列表：状态筛选（空串不筛选）+ 已校验分页，返回行与总数。
/// 输出全列（ENUM/JSON 列 CAST AS CHAR），id/user_id/order_id 转字符串，
/// images_json 解析为数组改名 images（缺省 []）。
pub async fn list(
    pool: &MySqlPool,
    status: &str,
    page: u32,
    page_size: u32,
) -> Result<(Vec<Value>, i64), FeedbackError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `feedback` WHERE `deleted_at` IS NULL AND (? = '' OR `status` = ?)",
    )
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    // 评价列表投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type FeedbackRow = (
        u64,
        u64,
        Option<u64>,
        Option<String>,
        Option<u8>,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<u64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<FeedbackRow> = sqlx::query_as(
        "SELECT `id`, `user_id`, `order_id`, `device_id`, `rating`, \
                CAST(`category` AS CHAR) AS `category`, `content`, \
                CAST(`images_json` AS CHAR) AS `images_json`, \
                CAST(`status` AS CHAR) AS `status`, `replied_by`, \
                DATE_FORMAT(`replied_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `replied_at`, \
                `reply_content`, \
                DATE_FORMAT(`created_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `created_at`, \
                DATE_FORMAT(`deleted_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `deleted_at` \
         FROM `feedback` WHERE `deleted_at` IS NULL AND (? = '' OR `status` = ?) \
         ORDER BY `id` DESC LIMIT ? OFFSET ?",
    )
    .bind(status)
    .bind(status)
    .bind(page_size as i64)
    .bind(((page - 1) as i64) * (page_size as i64))
    .fetch_all(pool)
    .await?;
    let items = rows
        .into_iter()
        .map(
            |(
                id,
                user_id,
                order_id,
                device_id,
                rating,
                category,
                content,
                images_json,
                status,
                replied_by,
                replied_at,
                reply_content,
                created_at,
                deleted_at,
            )| {
                let images = images_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .unwrap_or_else(|| json!([]));
                json!({
                    "id": id.to_string(),
                    "user_id": user_id.to_string(),
                    "order_id": order_id.map(|v| v.to_string()),
                    "device_id": device_id,
                    "rating": rating,
                    "category": category,
                    "content": content,
                    "images": images,
                    "status": status,
                    "replied_by": replied_by,
                    "replied_at": replied_at,
                    "reply_content": reply_content,
                    "created_at": created_at,
                    "deleted_at": deleted_at,
                })
            },
        )
        .collect();
    Ok((items, total))
}

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置；调用方随后
/// pool.acquire + conn.begin 开事务——连接级设置随连接保留）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}

/// 锁反馈行读状态（FOR UPDATE）；已删除行视为不存在。
pub async fn lock_feedback_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<Option<FeedbackStatus>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `status` FROM `feedback` \
         WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(c)
    .await
}

/// 回复反馈：pending → processed 并写回复内容与回复人。
pub async fn update_reply_tx(
    c: &mut MySqlConnection,
    id: u64,
    reply_content: &str,
    replied_by: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `feedback` SET `status` = ?, `reply_content` = ?, \
         `replied_by` = ?, `replied_at` = UTC_TIMESTAMP(3) WHERE `id` = ?",
    )
    .bind(FeedbackStatus::Processed)
    .bind(reply_content)
    .bind(replied_by)
    .bind(id)
    .execute(c)
    .await?;
    Ok(())
}

/// 关闭反馈：任意未关闭状态 → closed。
pub async fn update_status_tx(c: &mut MySqlConnection, id: u64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `feedback` SET `status` = ? WHERE `id` = ?")
        .bind(FeedbackStatus::Closed)
        .bind(id)
        .execute(c)
        .await?;
    Ok(())
}

/// 按业务单号查本人未删除订单主键；纯数字路径不经此查询。
pub async fn find_owned_order_id(
    pool: &MySqlPool,
    order_no: &str,
    user_id: u64,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `id` FROM `charge_order` \
         WHERE `order_no` = ? AND `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(order_no)
    .bind(user_id as i64)
    .fetch_optional(pool)
    .await
}

/// 锁订单行（FOR UPDATE）：返回 (归属用户, 设备, 状态)；不存在返回 None。
pub async fn lock_order_tx(
    tx: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<(u64, Option<String>, ChargeOrderStatus)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `user_id`, `device_id`, `status` \
         FROM `charge_order` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(tx)
    .await
}

/// 当前读核对已有反馈（FOR SHARE，防止旧快照漏掉并发提交）。
pub async fn feedback_count_share_tx(
    tx: &mut MySqlConnection,
    order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `feedback` \
         WHERE `order_id` = ? AND `deleted_at` IS NULL FOR SHARE",
    )
    .bind(order_id as i64)
    .fetch_one(tx)
    .await
}

/// 插入初始 pending 反馈；返回新反馈自增主键。
#[allow(clippy::too_many_arguments)]
pub async fn insert_feedback_tx(
    tx: &mut MySqlConnection,
    user_id: u64,
    order_id: u64,
    device_id: Option<String>,
    rating: i8,
    category: chargepilot_domain::order::FeedbackCategory,
    content: Option<&str>,
    images_json: &str,
) -> Result<u64, sqlx::Error> {
    let id = sqlx::query(
        "INSERT INTO `feedback` \
         (`user_id`, `order_id`, `device_id`, `rating`, `category`, `content`, `images_json`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(user_id as i64)
    .bind(order_id as i64)
    .bind(device_id)
    .bind(rating)
    .bind(category)
    .bind(content)
    .bind(images_json)
    .execute(tx)
    .await?
    .last_insert_id() as u64;
    Ok(id)
}
