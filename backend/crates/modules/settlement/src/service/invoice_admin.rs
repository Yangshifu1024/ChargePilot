//! 发票申请后台队列与双审（旧版 ListInvoiceRequests + InvoiceReviewRepo.Review 的单体对应物）。
//! 列表为全量倒序（旧版无分页参数），user_id 与审核人 ID 输出十进制字符串防止 JS 精度丢失。
//! 双审：通过须两名不同且仍有 invoice.review 权限的 customer_finance，两次提交的发票链接一致；
//! 驳回单人即可。审核表、申请单状态与审计同事务提交；同人复核与状态不符一律冲突。
//! SQL 在 repo/invoice_admin.rs。

use crate::error::InvoiceAdminError;
use crate::repo::{invoice_admin as repo, set_read_committed};
use chargepilot_domain::settlement::InvoiceRequestReviewStatus;
use chargepilot_mod_ops::Profile;
use sqlx::{Acquire, MySqlPool};

pub use crate::repo::invoice_admin::InvoiceAdminRow;

/// 执行发票通过或驳回（旧版 InvoiceReviewService.Review + InvoiceReviewRepo.Review）。
/// 审核人角色与权限在本函数内实时复核；二审通过前用事务内一致读取复查首位
/// 审核人的当前角色与权限（旧版 LookupReviewer 语义）。
/// 返回决定后的队列状态：awaiting_second / approved / rejected。
#[allow(clippy::too_many_arguments)]
pub async fn review_invoice(
    db: &MySqlPool,
    actor: &Profile,
    id: u64,
    approve: bool,
    invoice_url: &str,
    reason: &str,
    client_ip: &str,
    request_id: &str,
) -> Result<&'static str, InvoiceAdminError> {
    // 服务内实时复核（旧版 audit.Actor.Role / HasPermission 判定）。
    if actor.role != "customer_finance" || !actor.permissions.iter().any(|p| p == "invoice.review")
    {
        return Err(InvoiceAdminError::Forbidden);
    }
    let mut conn = db.acquire().await?;
    set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let Some((request_status, request_url)) = repo::lock_invoice_request_tx(&mut *c, id).await?
    else {
        return Err(InvoiceAdminError::NotFound);
    };
    let before = serde_json::json!({
        "review_status": request_status,
        "invoice_url": request_url,
    });
    if request_status != InvoiceRequestReviewStatus::Pending {
        return Err(InvoiceAdminError::Conflict);
    }
    let review = repo::find_admin_review_tx(&mut *c, id).await?;
    let after = serde_json::json!({
        "invoice_url": invoice_url,
        "reason": reason,
    });
    if !approve {
        // 尚无审核记录或已首审待复核时允许单人驳回。
        if let Some((review_status, _, _, _)) = &review
            && *review_status
                != chargepilot_domain::settlement::InvoiceAdminReviewStatus::AwaitingSecond
        {
            return Err(InvoiceAdminError::Conflict);
        }
        repo::reject_admin_review_tx(&mut *c, id, reason).await?;
        repo::mark_request_rejected_tx(&mut *c, id, actor.id, reason).await?;
        append_audit(
            &mut tx, actor, "rejected", id, &before, &after, client_ip, request_id,
        )
        .await?;
        tx.commit().await?;
        return Ok("rejected");
    }
    let status: &'static str = match &review {
        // 首审：登记首审人与发票链接，等待第二人复核。
        None => {
            repo::insert_first_review_tx(&mut *c, id, actor.id, invoice_url).await?;
            "awaiting_second"
        }
        Some((review_status, first_reviewer_id, _, review_url)) => {
            let Some(first) = first_reviewer_id else {
                // 审核记录存在却无首审人：状态不完整，拒绝（旧版会走到重复主键失败）。
                return Err(InvoiceAdminError::Conflict);
            };
            if *first == actor.id
                || *review_status
                    != chargepilot_domain::settlement::InvoiceAdminReviewStatus::AwaitingSecond
                || review_url.is_none()
                || review_url.as_deref() != Some(invoice_url)
            {
                return Err(InvoiceAdminError::Conflict);
            }
            // 复查首位审核人的当前角色与权限（不信任首审时保存的身份）。
            // 旧版经注入的 LookupReviewer 查询；此处用事务内一致读取（权限 FOR SHARE）。
            let first_profile = chargepilot_mod_ops::repo::load_profile_tx(&mut *c, *first)
                .await
                .map_err(|_| InvoiceAdminError::Conflict)?;
            if first_profile.role != "customer_finance"
                || !first_profile
                    .permissions
                    .iter()
                    .any(|p| p == "invoice.review")
            {
                return Err(InvoiceAdminError::Conflict);
            }
            repo::approve_admin_review_tx(&mut *c, id, actor.id).await?;
            repo::mark_request_issued_tx(&mut *c, id, invoice_url, actor.id).await?;
            "approved"
        }
    };
    append_audit(
        &mut tx, actor, status, id, &before, &after, client_ip, request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(status)
}

/// 审核队列的审计写入（旧版 AppendAuditInTx：module/target 同为 invoice）。
#[allow(clippy::too_many_arguments)]
async fn append_audit(
    tx: &mut sqlx::MySqlConnection,
    actor: &Profile,
    action: &str,
    id: u64,
    before: &serde_json::Value,
    after: &serde_json::Value,
    client_ip: &str,
    request_id: &str,
) -> Result<(), InvoiceAdminError> {
    chargepilot_mod_ops::repo::append_audit_tx(
        tx,
        actor,
        "invoice",
        action,
        "invoice",
        &id.to_string(),
        before,
        after,
        client_ip,
        request_id,
    )
    .await
    .map_err(|e| InvoiceAdminError::Storage(e.to_string()))
}
