//! 用户侧在线卡：列表、挂失与解绑。SQL 在 repo/user_card.rs。
//!
//! 状态语义（online_card.status，active/lost/disabled/unbound）：
//! C 端自助入口只允许写入 lost（挂失）与 unbound（解绑）——启用/停用仅运营
//! 可写（admin.rs）。查询与锁读都排除已解绑（`status <> 'unbound'`）：
//! 解绑即从用户视角消失，不可再被改回任何状态。
//!
//! 变更事务：行锁读卡（卡号 + 本人 + 未解绑）
//! → 写 status → 写 online_card_audit（action = `user_<status>`），同事务提交，
//! 失败整体回滚；不结束现有充电，不动余额。

use crate::error::UserCardError;
use crate::repo::{set_card_status_tx, set_read_committed, user_card as card_repo};
use serde_json::json;
use sqlx::{Acquire, MySqlPool};

/// 返回用户本人尚未解绑的卡。
/// 输出形状
/// `user_id` 十进制字符串、`status` 字符串。
pub async fn list(pool: &MySqlPool, user_id: u64) -> Result<Vec<serde_json::Value>, UserCardError> {
    let rows = card_repo::owned_card_rows(pool, user_id).await?;
    Ok(rows
        .into_iter()
        .map(|(id, card_no, owner, status)| {
            json!({
                "id": id,
                "card_no": card_no,
                "user_id": owner.to_string(),
                "status": status,
            })
        })
        .collect())
}

/// 将本人所属卡挂失（lost）或解绑（unbound）。
/// 归属与存在性核对在锁读中完成：卡不存在、属于他人或已解绑统一 NotFound。
pub async fn change_status(
    pool: &MySqlPool,
    user_id: u64,
    card_no: &str,
    status: &str,
) -> Result<(), UserCardError> {
    // C 端可写子集白名单：仅挂失与解绑。
    if status != "lost" && status != "unbound" {
        return Err(UserCardError::Input);
    }
    let mut conn = pool.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let Some(card_id) = card_repo::lock_user_card_tx(&mut tx, card_no, user_id).await? else {
        return Err(UserCardError::NotFound);
    };
    set_card_status_tx(&mut tx, card_id, status).await?;
    // 用户操作审计同事务。
    crate::repo::admin::append_audit_tx(
        &mut tx,
        card_id,
        user_id,
        &format!("user_{status}"),
        &format!("用户主动{status}"),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
