//! 运营后台在线卡管理：列表、核验绑定与状态变更（旧版 card/admin.go +
//! card_admin_repo.go 对齐）。绑定与状态变更都是事务：先校验用户，
//! 再以卡号唯一键创建或行锁核对归属；状态写入与审计同事务提交，
//! 失败整体回滚。卡绑定用户而不绑定站点，不迁移钱包余额。

use chargepilot_mod_ops::Profile;
use serde::Serialize;
use sqlx::{Acquire, MySqlConnection, MySqlPool};

/// 卡号长度上限（旧版 money.MaxPANLength；数据库列长 64 不放宽业务校验）。
const MAX_CARD_NO_LEN: usize = 19;

/// 绑卡冲突原因（旧版 ErrCardAlreadyBound 文案，随 409 透出）。
pub const ALREADY_BOUND: &str = "卡已绑定，须先由原用户解绑";

/// 裸业务冲突文案（旧版 operations.ErrConflict.Error()，随 409 透出）。
const CONFLICT_PLAIN: &str = "当前记录状态不允许此操作，请刷新后重试";

/// 后台在线卡错误（HTTP 层翻译状态码）。
#[derive(Debug, thiserror::Error)]
pub enum CardAdminError {
    /// 绑定或状态输入不满足业务约束（400，文案由处理器按端点给出）。
    #[error("card admin input invalid")]
    Input,
    /// 实时权限不足（403/1003）。
    #[error("当前账号无权执行此操作")]
    Forbidden,
    /// 记录当前状态拒绝写入（409，文案透出）。
    #[error("{0}")]
    Conflict(String),
    /// 卡或目标用户不存在（404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储不可用（503/5003）。
    #[error("存储不可用: {0}")]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for CardAdminError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 在线卡行（旧版 OnlineCard 的 JSON 契约：id 数字、user_id 十进制字符串）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OnlineCard {
    pub id: u64,
    /// 绑定卡号的规范十进制文本（ascii_bin 列读侧 CAST AS CHAR）。
    pub card_no: String,
    /// 绑定用户主键，响应按十进制字符串传输（旧版 `,string`）。
    pub user_id: String,
    /// online_card.status：active/lost/disabled/unbound。
    pub status: String,
}

/// 校验规范卡号：1–19 位十进制、拒绝前导零（旧版 money.CanonicalCardNumber）。
fn canonical_card_number(card: &str) -> bool {
    !card.is_empty()
        && card.len() <= MAX_CARD_NO_LEN
        && card.as_bytes()[0] != b'0'
        && card.bytes().all(|b| b.is_ascii_digit())
}

/// 自由文本校验：去首尾空白后非空，按字符计数不超 max（旧版 operations.ValidText）。
fn valid_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max
}

fn require_manage(actor: &Profile) -> Result<(), CardAdminError> {
    if actor.permissions.iter().any(|p| p == "online_card.manage") {
        Ok(())
    } else {
        Err(CardAdminError::Forbidden)
    }
}

fn require_read(actor: &Profile) -> Result<(), CardAdminError> {
    if actor.permissions.iter().any(|p| p == "user.read") {
        Ok(())
    } else {
        Err(CardAdminError::Forbidden)
    }
}

/// 按实时 user.read 权限返回最新 500 张卡（旧版 List；在线卡是全局功能，不分页不过滤）。
pub async fn list_cards(
    pool: &MySqlPool,
    actor: &Profile,
) -> Result<Vec<OnlineCard>, CardAdminError> {
    require_read(actor)?;
    let rows = sqlx::query_as::<_, OnlineCard>(
        "SELECT `id`, CAST(`card_no` AS CHAR) AS `card_no`, \
                CAST(`user_id` AS CHAR) AS `user_id`, CAST(`status` AS CHAR) AS `status` \
         FROM `online_card` ORDER BY `id` DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 事务内确认绑定目标用户未删除且启用（旧版 identity.RequireActiveUserInTx 语义）；
/// 不符合时返回 NotFound（旧版经 gorm.ErrRecordNotFound 映射 404）。
async fn require_active_user_tx(
    conn: &mut MySqlConnection,
    user_id: u64,
) -> Result<(), CardAdminError> {
    // id 列为 BIGINT UNSIGNED，读侧按 u64 解码。
    let found: Option<u64> = sqlx::query_scalar(
        "SELECT `id` FROM `user` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL",
    )
    .bind(user_id as i64)
    .fetch_optional(conn)
    .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(CardAdminError::NotFound),
    }
}

/// 在已锁卡的连接中更新归属并回到 active（旧版 BindOnlineCard）。
async fn bind_online_card_tx(
    conn: &mut MySqlConnection,
    id: u64,
    user_id: u64,
) -> Result<(), CardAdminError> {
    sqlx::query("UPDATE `online_card` SET `user_id` = ?, `status` = 'active' WHERE `id` = ?")
        .bind(user_id as i64)
        .bind(id as i64)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// 在调用方事务中保存一条卡审计（旧版 AppendOnlineCardAudit）；失败随事务回滚。
async fn append_audit_tx(
    conn: &mut MySqlConnection,
    card_id: u64,
    actor_id: u64,
    action: &str,
    detail: &str,
) -> Result<(), CardAdminError> {
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

/// 核验绑定（旧版 Bind）：先校验用户，再通过卡号唯一键创建或加锁核对归属；
/// 同用户已启用的卡重放不重复审计；绑定与审计同事务，拒绝已绑定他人的卡，
/// 失败全部回滚。返回卡 ID。
pub async fn bind_card(
    pool: &MySqlPool,
    actor: &Profile,
    card_no: &str,
    user_id: u64,
    reason: &str,
) -> Result<u64, CardAdminError> {
    require_manage(actor)?;
    let card_no = card_no.trim();
    if !canonical_card_number(card_no) || user_id == 0 || !valid_text(reason, 500) {
        return Err(CardAdminError::Input);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    require_active_user_tx(c, user_id).await?;
    // 唯一键首次绑定：重复键不改归属（旧版 ClaimBindingInTx 的 INSERT IGNORE），
    // 避免对缺失卡先取间隙锁后并发插入的死锁。
    let created = sqlx::query(
        "INSERT IGNORE INTO `online_card` (`card_no`, `user_id`, `status`) VALUES (?, ?, 'active')",
    )
    .bind(card_no)
    .bind(user_id as i64)
    .execute(&mut *c)
    .await?
    .rows_affected()
        == 1;
    // 行锁核对归属（旧版 LockCardByNo）：首次绑定也已插行，缺失即存储异常。
    let row: Option<(u64, u64, chargepilot_domain::card::CardStatus)> = sqlx::query_as(
        "SELECT `id`, `user_id`, `status` FROM `online_card` \
         WHERE `card_no` = ? FOR UPDATE",
    )
    .bind(card_no)
    .fetch_optional(&mut *c)
    .await?;
    let (id, locked_user, status) = row.ok_or(CardAdminError::NotFound)?;
    if !created {
        if status != chargepilot_domain::card::CardStatus::Unbound {
            if locked_user == user_id && status == chargepilot_domain::card::CardStatus::Active {
                // 同用户已启用：幂等重放，提交空事务且不重复审计。
                tx.commit().await?;
                return Ok(id);
            }
            tx.rollback().await?;
            return Err(CardAdminError::Conflict(ALREADY_BOUND.to_string()));
        }
        // 已解绑卡重新绑定原用户或新用户。
        bind_online_card_tx(c, id, user_id).await?;
    }
    append_audit_tx(c, id, actor.id, "bind", reason).await?;
    tx.commit().await?;
    Ok(id)
}

/// 修改卡状态（旧版 ChangeStatus）：状态与核验审计同事务；
/// 已解绑卡不能恢复其他状态；不影响正在充电的订单或钱包余额。
pub async fn change_card_status(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
    status: &str,
    reason: &str,
) -> Result<(), CardAdminError> {
    require_manage(actor)?;
    // 运营端可写子集为四值全开；解绑不可逆的约束在事务内单独判定。
    if !matches!(status, "active" | "lost" | "disabled" | "unbound") || !valid_text(reason, 500) {
        return Err(CardAdminError::Input);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let row: Option<(u64, String)> = sqlx::query_as(
        "SELECT `id`, CAST(`status` AS CHAR) FROM `online_card` WHERE `id` = ? FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let (_, current) = row.ok_or(CardAdminError::NotFound)?;
    if current == "unbound" && status != "unbound" {
        tx.rollback().await?;
        return Err(CardAdminError::Conflict(CONFLICT_PLAIN.to_string()));
    }
    sqlx::query("UPDATE `online_card` SET `status` = ? WHERE `id` = ?")
        .bind(status)
        .bind(id as i64)
        .execute(&mut *tx)
        .await?;
    append_audit_tx(&mut tx, id, actor.id, status, reason).await?;
    tx.commit().await?;
    Ok(())
}
