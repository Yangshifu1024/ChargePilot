//! 运营后台在线卡管理：列表、核验绑定与状态变更（旧版 card/admin.go +
//! card_admin_repo.go 对齐）。绑定与状态变更都是事务：先校验用户，
//! 再以卡号唯一键创建或行锁核对归属；状态写入与审计同事务提交，
//! 失败整体回滚。卡绑定用户而不绑定站点，不迁移钱包余额。SQL 在 repo/admin.rs。

use crate::error::CardAdminError;
use crate::repo::{admin as card_repo, set_card_status_tx, set_read_committed};
use chargepilot_mod_ops::Profile;
use sqlx::{Acquire, MySqlPool};

pub use crate::repo::admin::OnlineCard;

/// 卡号长度上限（旧版 money.MaxPANLength；数据库列长 64 不放宽业务校验）。
const MAX_CARD_NO_LEN: usize = 19;

/// 绑卡冲突原因（旧版 ErrCardAlreadyBound 文案，随 409 透出）。
pub const ALREADY_BOUND: &str = "卡已绑定，须先由原用户解绑";

/// 裸业务冲突文案（旧版 operations.ErrConflict.Error()，随 409 透出）。
const CONFLICT_PLAIN: &str = "当前记录状态不允许此操作，请刷新后重试";

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
    Ok(card_repo::admin_card_rows(pool).await?)
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
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    // 事务内确认绑定目标用户未删除且启用（旧版 identity.RequireActiveUserInTx
    // 语义；不符合时返回 NotFound，旧版经 gorm.ErrRecordNotFound 映射 404）。
    if card_repo::active_user_id_tx(c, user_id).await?.is_none() {
        return Err(CardAdminError::NotFound);
    }
    let created = card_repo::insert_ignore_card_tx(c, card_no, user_id).await?;
    // 行锁核对归属（旧版 LockCardByNo）：首次绑定也已插行，缺失即存储异常。
    let (id, locked_user, status) = card_repo::lock_card_by_no_tx(c, card_no)
        .await?
        .ok_or(CardAdminError::NotFound)?;
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
        card_repo::bind_online_card_tx(c, id, user_id).await?;
    }
    card_repo::append_audit_tx(c, id, actor.id, "bind", reason).await?;
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
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let (_, current) = card_repo::lock_card_status_tx(&mut tx, id)
        .await?
        .ok_or(CardAdminError::NotFound)?;
    if current == "unbound" && status != "unbound" {
        tx.rollback().await?;
        return Err(CardAdminError::Conflict(CONFLICT_PLAIN.to_string()));
    }
    set_card_status_tx(&mut tx, id, status).await?;
    card_repo::append_audit_tx(&mut tx, id, actor.id, status, reason).await?;
    tx.commit().await?;
    Ok(())
}
