//! 用户侧在线卡的表访问：online_card（蓝图第五章 repo 层）。

use sqlx::{MySqlConnection, MySqlPool};

/// 本人尚未解绑的卡（旧版 UserCardRepo.List）。
/// 输出形状对齐旧版 OnlineCard JSON：id 数字、card_no 字符串、user_id 数值、status 字符串。
pub async fn owned_card_rows(
    pool: &MySqlPool,
    user_id: u64,
) -> Result<Vec<(u64, String, u64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, CAST(`card_no` AS CHAR) AS `card_no`, `user_id`, \
                CAST(`status` AS CHAR) AS `status` \
         FROM `online_card` WHERE `user_id` = ? AND `status` <> 'unbound' ORDER BY `id`",
    )
    .bind(user_id as i64)
    .fetch_all(pool)
    .await
}

/// 锁读：卡号 + 本人 + 未解绑，避免先读再核对归属的竞态（旧版 LockUserCard）。
pub async fn lock_user_card_tx(
    conn: &mut MySqlConnection,
    card_no: &str,
    user_id: u64,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `id` FROM `online_card` \
         WHERE `card_no` = ? AND `user_id` = ? AND `status` <> 'unbound' FOR UPDATE",
    )
    .bind(card_no)
    .bind(user_id as i64)
    .fetch_optional(&mut *conn)
    .await
}
