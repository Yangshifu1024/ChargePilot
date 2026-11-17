//! 后台在线卡的表访问：online_card / online_card_audit / user（蓝图第五章 repo 层）。

use chargepilot_domain::card::CardStatus;
use sqlx::MySqlConnection;

/// 在线卡行。
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct OnlineCard {
    pub id: u64,
    /// 绑定卡号的规范十进制文本（ascii_bin 列读侧 CAST AS CHAR）。
    pub card_no: String,
    /// 绑定用户主键，响应按十进制字符串传输。
    pub user_id: String,
    /// online_card.status：active/lost/disabled/unbound。
    pub status: String,
}

/// 最新 500 张卡（在线卡是全局功能，不分页不过滤）。
pub async fn admin_card_rows(pool: &sqlx::MySqlPool) -> Result<Vec<OnlineCard>, sqlx::Error> {
    sqlx::query_as::<_, OnlineCard>(
        "SELECT `id`, CAST(`card_no` AS CHAR) AS `card_no`, \
                CAST(`user_id` AS CHAR) AS `user_id`, CAST(`status` AS CHAR) AS `status` \
         FROM `online_card` ORDER BY `id` DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await
}

/// 事务内确认绑定目标用户未删除且启用。
/// id 列为 BIGINT UNSIGNED，读侧按 u64 解码。
pub async fn active_user_id_tx(
    conn: &mut MySqlConnection,
    user_id: u64,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT `id` FROM `user` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *conn)
    .await
}

/// 在已锁卡的连接中更新归属并回到 active。
pub async fn bind_online_card_tx(
    conn: &mut MySqlConnection,
    id: u64,
    user_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `online_card` SET `user_id` = ?, `status` = 'active' WHERE `id` = ?")
        .bind(user_id as i64)
        .bind(id as i64)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// 在调用方事务中保存一条卡审计；失败随事务回滚。
/// 后台与用户侧审计同表同形态（action 由调用方区分）。
pub async fn append_audit_tx(
    conn: &mut MySqlConnection,
    card_id: u64,
    actor_id: u64,
    action: &str,
    detail: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `online_card_audit` (`card_id`, `actor_id`, `action`, `detail`) VALUES (?, ?, ?, ?)",
    )
    .bind(card_id as i64)
    .bind(actor_id as i64)
    .bind(action)
    .bind(detail)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 唯一键首次绑定：重复键不改归属，
/// 避免对缺失卡先取间隙锁后并发插入的死锁；返回是否为本次新建。
pub async fn insert_ignore_card_tx(
    conn: &mut MySqlConnection,
    card_no: &str,
    user_id: u64,
) -> Result<bool, sqlx::Error> {
    let created = sqlx::query(
        "INSERT IGNORE INTO `online_card` (`card_no`, `user_id`, `status`) VALUES (?, ?, 'active')",
    )
    .bind(card_no)
    .bind(user_id as i64)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    Ok(created)
}

/// 行锁按卡号核对归属：首次绑定也已插行，缺失即存储异常。
pub async fn lock_card_by_no_tx(
    conn: &mut MySqlConnection,
    card_no: &str,
) -> Result<Option<(u64, u64, CardStatus)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, `user_id`, `status` FROM `online_card` \
         WHERE `card_no` = ? FOR UPDATE",
    )
    .bind(card_no)
    .fetch_optional(&mut *conn)
    .await
}

/// 行锁读卡当前状态（状态变更前置判定）。
pub async fn lock_card_status_tx(
    conn: &mut MySqlConnection,
    id: u64,
) -> Result<Option<(u64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `id`, CAST(`status` AS CHAR) FROM `online_card` WHERE `id` = ? FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *conn)
    .await
}
