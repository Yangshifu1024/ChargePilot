//! 后台账号与角色管理用例：锁顺序固定为「customer_admin 角色行 → 操作者 →
//! 目标行」，最后管理员保护与越权校验在锁内复核。审计与业务同事务提交。
//! SQL 原语在 repo/management.rs。

use crate::repo;
use crate::repo::management;
use crate::{OpsError, Profile};
use serde_json::{Value, json};
use sqlx::{MySqlConnection, MySqlPool};

pub use crate::repo::management::{AdminUserRow, PermissionRow, RoleRow};

const MODULE_ADMIN_USER: &str = "admin_user";
const MODULE_ROLE: &str = "role";

/// 分页参数（page≥1、page_size 1–100 默认 20）。
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub page: u32,
    pub page_size: u32,
}

impl Page {
    pub fn sanitized(page: i64, page_size: i64) -> anyhow::Result<Self> {
        // 双轨上界（后台 100 万）与字段级文案经 domain::page 统一。
        let (page, page_size) = match chargepilot_domain::page::PageRequest::admin(page, page_size)
        {
            Ok(p) => (p.page, p.page_size),
            Err(chargepilot_domain::page::PageBoundsField::Page) => {
                anyhow::bail!("分页参数无效");
            }
            Err(chargepilot_domain::page::PageBoundsField::PageSize) => {
                anyhow::bail!("分页参数无效：page_size 为 1–100");
            }
        };
        Ok(Self { page, page_size })
    }

    fn offset(&self) -> i64 {
        ((self.page - 1) as i64) * (self.page_size as i64)
    }
}

// ---------------------------------------------------------------------------
// 账号管理
// ---------------------------------------------------------------------------

pub async fn list_admin_users(
    pool: &MySqlPool,
    page: Page,
    keyword: Option<&str>,
    status: Option<&str>,
) -> anyhow::Result<(Vec<AdminUserRow>, i64)> {
    let kw = format!(
        "%{}%",
        keyword
            .unwrap_or_default()
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    );
    let total = management::admin_count(pool, &kw, status).await?;
    let rows =
        management::admin_page(pool, &kw, status, page.page_size as i64, page.offset()).await?;
    Ok((rows, total))
}

/// 创建账号。校验 → 集合锁 → 锁操作者 → 越权校验 → 写入+审计（同事务）。
pub struct CreateAdminInput {
    pub username: String,
    pub display_name: Option<String>,
    pub password: String,
    pub role_id: u64,
    pub phone: Option<String>,
}

pub async fn create_admin(
    pool: &MySqlPool,
    actor: &Profile,
    input: CreateAdminInput,
    ip: &str,
    request_id: &str,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> Result<u64, OpsError> {
    let username_ok = (3..=64).contains(&input.username.len())
        && input
            .username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !username_ok
        || input
            .display_name
            .as_ref()
            .is_some_and(|d| d.chars().count() > 64)
        || !(chargepilot_domain::limits::PASSWORD_LEN).contains(&input.password.len())
        || input.role_id == 0
        || input.phone.as_ref().is_some_and(|p| p.len() > 32)
    {
        return Err(OpsError::bad_input(
            "用户名须为 3–64 位英文/数字，密码须为 12–72 字节，并选择有效角色",
        ));
    }
    let hash = chargepilot_platform::password::hash(&input.password).map_err(OpsError::from)?;
    let new_id = allocator.next_id().await.map_err(OpsError::from)?;
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    ensure_grant_allowed_tx(&mut tx, &actor_locked, &[input.role_id]).await?;
    management::insert_admin_tx(
        &mut tx,
        new_id,
        &input.username,
        &input.display_name,
        hash,
        input.role_id,
        &input.phone,
    )
    .await
    .map_err(|e| duplicate_or(e, "用户名已被使用"))?;
    let after = json!({"id": new_id, "username": input.username, "role_id": input.role_id, "status": "active"});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ADMIN_USER,
        "create",
        "admin_user",
        &new_id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(new_id)
}

/// 更新账号（指针字段 nil=不修改）。角色或状态变更时递增 auth_version。
pub struct UpdateAdminInput {
    pub display_name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub role_id: Option<u64>,
    pub status: Option<String>,
}

pub async fn update_admin(
    pool: &MySqlPool,
    actor: &Profile,
    target_id: u64,
    input: UpdateAdminInput,
    ip: &str,
    request_id: &str,
) -> Result<(u64, bool), OpsError> {
    if input
        .display_name
        .as_ref()
        .is_some_and(|d| d.chars().count() > 128)
    {
        return Err(OpsError::bad_input("显示名称过长"));
    }
    if input.phone.as_ref().is_some_and(|p| p.len() > 32) {
        return Err(OpsError::bad_input("手机号过长"));
    }
    if let Some(e) = input.email.as_deref() {
        if (e.is_empty() || !e.contains('@')) && e.chars().count() > 128 {
            return Err(OpsError::bad_input("邮箱格式无效"));
        }
        if !e.is_empty() && !e.contains('@') {
            return Err(OpsError::bad_input("邮箱格式无效"));
        }
    }
    if let Some(s) = &input.status {
        // 仅允许设置 active/disabled；locked 是登录风控专用状态。
        match chargepilot_domain::operations::AdminUserStatus::parse(s) {
            Ok(
                chargepilot_domain::operations::AdminUserStatus::Active
                | chargepilot_domain::operations::AdminUserStatus::Disabled,
            ) => {}
            _ => return Err(OpsError::bad_input("账号状态无效")),
        }
    }
    if input.display_name.is_none()
        && input.phone.is_none()
        && input.email.is_none()
        && input.role_id.is_none()
        && input.status.is_none()
    {
        return Err(OpsError::bad_input("没有可更新的字段"));
    }
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let target = repo::lock_admin_tx(&mut tx, target_id)
        .await
        .map_err(OpsError::from)?
        .ok_or(OpsError::Credentials)?;
    if let Some(rid) = input.role_id {
        ensure_grant_allowed_tx(&mut tx, &actor_locked, &[rid]).await?;
    }
    let role_changed = input
        .role_id
        .is_some_and(|r| r != target.role_id.unwrap_or(0));
    let status_changed = input.status.as_deref().is_some_and(|s| {
        chargepilot_domain::operations::AdminUserStatus::parse(s)
            .map(|parsed| parsed != target.status)
            .unwrap_or(true)
    });
    if target.id == actor.id && (status_changed || role_changed) {
        return Err(OpsError::conflict("不能修改自己的账号状态或角色"));
    }
    let mut sessions_revoked = false;
    if let Some(rid) = input.role_id {
        management::set_admin_role_tx(&mut tx, rid, target.id).await?;
        sessions_revoked = true;
    }
    if let Some(s) = &input.status {
        let status = chargepilot_domain::operations::AdminUserStatus::parse(s)
            .map_err(|_| OpsError::bad_input("账号状态无效"))?;
        management::set_admin_status_tx(&mut tx, status, target.id).await?;
        sessions_revoked = true;
    }
    if let Some(d) = &input.display_name {
        management::set_admin_display_name_tx(&mut tx, d, target.id).await?;
    }
    if let Some(p) = &input.phone {
        management::set_admin_phone_tx(&mut tx, p, target.id).await?;
    }
    if let Some(e) = &input.email {
        management::set_admin_email_tx(&mut tx, e, target.id).await?;
    }
    // 最后管理员保护：目标是 active 的 customer_admin 且无其他有效客户管理员 → 拒绝
    if (status_changed && input.status.as_deref() == Some("disabled") || role_changed)
        && is_customer_admin_role(&target.role_id)
    {
        let others = repo::count_other_active_customer_admins_tx(&mut tx, target.id)
            .await
            .map_err(OpsError::from)?;
        if others == 0 {
            return Err(OpsError::conflict("必须保留至少一个有效的客户管理员账号"));
        }
    }
    let before = admin_snapshot(&target);
    let updated = repo::lock_admin_tx(&mut tx, target.id)
        .await
        .map_err(OpsError::from)?;
    let after = updated.as_ref().map(admin_snapshot).unwrap_or(Value::Null);
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ADMIN_USER,
        "update",
        "admin_user",
        &target.id.to_string(),
        &before,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok((target.id, sessions_revoked))
}

pub async fn delete_admin(
    pool: &MySqlPool,
    actor: &Profile,
    target_id: u64,
    ip: &str,
    request_id: &str,
) -> Result<u64, OpsError> {
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let target = repo::lock_admin_tx(&mut tx, target_id)
        .await
        .map_err(OpsError::from)?
        .ok_or_else(|| OpsError::bad_input("记录不存在或已删除"))?;
    if target.id == actor.id {
        return Err(OpsError::conflict("不能删除当前登录账号"));
    }
    if is_customer_admin_role(&target.role_id) {
        let others = repo::count_other_active_customer_admins_tx(&mut tx, target.id)
            .await
            .map_err(OpsError::from)?;
        if others == 0 {
            return Err(OpsError::conflict("必须保留至少一个有效的客户管理员账号"));
        }
    }
    management::soft_delete_admin_tx(&mut tx, actor.id, target.id).await?;
    let before = admin_snapshot(&target);
    let after = json!({"deleted": true});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ADMIN_USER,
        "delete",
        "admin_user",
        &target.id.to_string(),
        &before,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(target.id)
}

pub async fn unlock_admin(
    pool: &MySqlPool,
    actor: &Profile,
    target_id: u64,
    ip: &str,
    request_id: &str,
) -> Result<u64, OpsError> {
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let target = repo::lock_admin_tx(&mut tx, target_id)
        .await
        .map_err(OpsError::from)?
        .ok_or_else(|| OpsError::bad_input("记录不存在或已删除"))?;
    if target.status != chargepilot_domain::operations::AdminUserStatus::Active {
        management::unlock_admin_tx(&mut tx, target.id).await?;
    }
    let after = json!({"unlocked": true});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ADMIN_USER,
        "unlock",
        "admin_user",
        &target.id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(target.id)
}

pub async fn reset_admin_password(
    pool: &MySqlPool,
    actor: &Profile,
    target_id: u64,
    new_password: &str,
    ip: &str,
    request_id: &str,
) -> Result<u64, OpsError> {
    if !(chargepilot_domain::limits::PASSWORD_LEN).contains(&new_password.len()) {
        return Err(OpsError::bad_input("新密码须为 12–72 字节"));
    }
    let hash = chargepilot_platform::password::hash(new_password).map_err(OpsError::from)?;
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let target = repo::lock_admin_tx(&mut tx, target_id)
        .await
        .map_err(OpsError::from)?
        .ok_or_else(|| OpsError::bad_input("记录不存在或已删除"))?;
    // 仅原状态为 locked 时恢复 active（停用账号重置密码后仍停用）
    management::reset_password_tx(&mut tx, hash, target.id).await?;
    let after = json!({"sessions_revoked": true});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ADMIN_USER,
        "reset_password",
        "admin_user",
        &target.id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(target.id)
}

// ---------------------------------------------------------------------------
// 角色管理
// ---------------------------------------------------------------------------

pub async fn list_roles(pool: &MySqlPool) -> Result<Vec<RoleRow>, OpsError> {
    let mut roles = management::role_rows(pool).await.map_err(OpsError::from)?;
    for role in &mut roles {
        let codes = if role.id > 0 {
            repo::permission_codes(pool, role.id).await?
        } else {
            Vec::new()
        };
        role.permissions =
            Some(serde_json::to_value(repo::without_retired(codes)).unwrap_or(json!([])));
    }
    Ok(roles)
}

pub async fn list_permissions(pool: &MySqlPool) -> Result<Vec<PermissionRow>, OpsError> {
    management::permission_rows(pool).await
}

pub struct RoleInput {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub permissions: Vec<String>,
}

fn validate_role_shape(input: &RoleInput, for_create: bool) -> Result<(), OpsError> {
    let code_ok = !for_create || {
        let bytes = input.code.as_bytes();
        bytes.len() >= 3
            && bytes.len() <= 64
            && input.code.starts_with(|c: char| c.is_ascii_lowercase())
            && input
                .code
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let name_ok = !input.name.trim().is_empty() && input.name.chars().count() <= 128;
    let desc_ok = input
        .description
        .as_ref()
        .map(|d| d.chars().count() <= 255)
        .unwrap_or(true);
    let perms_ok = !input.permissions.is_empty()
        && input.permissions.iter().all(|p| !p.trim().is_empty())
        && input
            .permissions
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == input.permissions.len();
    if !code_ok || !name_ok || !desc_ok || !perms_ok {
        return Err(OpsError::bad_input(
            "角色无效：编码须为小写字母开头的 3–64 位，名称必填，权限须非空且只能从自己已持有的权限中选择",
        ));
    }
    Ok(())
}

pub async fn create_role(
    pool: &MySqlPool,
    actor: &Profile,
    input: RoleInput,
    ip: &str,
    request_id: &str,
) -> Result<u64, OpsError> {
    validate_role_shape(&input, true)?;
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    validate_grant_tx(&mut tx, &actor_locked, &input.permissions).await?;
    let id = management::next_role_id_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    management::insert_role_tx(
        &mut tx,
        id,
        &input.code,
        input.name.trim(),
        &input.description,
    )
    .await
    .map_err(|e| duplicate_or(e, "编码已存在，请使用其他编码"))?;
    management::replace_role_permissions_tx(&mut tx, id, &input.permissions)
        .await
        .map_err(OpsError::from)?;
    let after =
        json!({"id": id, "code": input.code, "name": input.name, "permissions": input.permissions});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ROLE,
        "create",
        "role",
        &id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(id)
}

pub async fn update_role(
    pool: &MySqlPool,
    actor: &Profile,
    role_id: u64,
    input: RoleInput,
    ip: &str,
    request_id: &str,
) -> Result<(u64, bool), OpsError> {
    validate_role_shape(&input, false)?;
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let existing = management::lock_role_name_tx(&mut tx, role_id)
        .await?
        .ok_or_else(|| OpsError::not_found("角色不存在"))?;
    let before_permissions: Vec<String> = if existing.2 == 1 {
        repo::permission_codes(pool, role_id)
            .await
            .map_err(OpsError::from)?
    } else {
        Vec::new()
    };
    let permissions_locked = existing.2 == 1;
    management::update_role_tx(&mut tx, role_id, input.name.trim(), &input.description).await?;
    if !permissions_locked {
        validate_grant_tx(&mut tx, &actor_locked, &input.permissions).await?;
        management::replace_role_permissions_tx(&mut tx, role_id, &input.permissions)
            .await
            .map_err(OpsError::from)?;
    }
    let after = if permissions_locked {
        json!({"id": role_id, "name": input.name, "permissions_locked": true, "permissions_unchanged": before_permissions})
    } else {
        json!({"id": role_id, "name": input.name, "permissions": input.permissions, "permissions_locked": false})
    };
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ROLE,
        "update",
        "role",
        &role_id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok((role_id, permissions_locked))
}

pub async fn delete_role(
    pool: &MySqlPool,
    actor: &Profile,
    role_id: u64,
    ip: &str,
    request_id: &str,
) -> Result<u64, OpsError> {
    let mut tx = pool.begin().await.map_err(OpsError::from)?;
    repo::lock_customer_admin_role_tx(&mut tx)
        .await
        .map_err(OpsError::from)?;
    let actor_locked = repo::lock_actor_tx(&mut tx, actor.id)
        .await
        .map_err(OpsError::from)?;
    let existing = management::lock_role_delete_tx(&mut tx, role_id)
        .await?
        .ok_or_else(|| OpsError::not_found("角色不存在"))?;
    if existing.1 == 1 {
        return Err(OpsError::conflict(
            "内置角色不可删除：它承载运营自身的访问权限",
        ));
    }
    if actor.role_id == role_id {
        return Err(OpsError::conflict("不能删除自己正在使用的角色"));
    }
    let users = management::role_user_count_tx(&mut tx, role_id).await?;
    if users > 0 {
        return Err(OpsError::conflict("该角色下仍有账号，请先改派后再删除"));
    }
    management::soft_delete_role_tx(&mut tx, role_id).await?;
    management::delete_role_permissions_tx(&mut tx, role_id)
        .await
        .map_err(OpsError::from)?;
    let after = json!({"id": role_id, "code": existing.2});
    repo::append_audit_tx(
        &mut tx,
        &actor_locked,
        MODULE_ROLE,
        "delete",
        "role",
        &role_id.to_string(),
        &Value::Null,
        &after,
        ip,
        request_id,
    )
    .await
    .map_err(OpsError::from)?;
    tx.commit().await.map_err(OpsError::from)?;
    Ok(role_id)
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

fn is_customer_admin_role(role_id: &Option<u64>) -> bool {
    // customer_admin 的内置 ID=1（种子固定）；范围外的自定义角色不参与保护
    matches!(role_id, Some(1))
}

async fn ensure_grant_allowed_tx(
    tx: &mut MySqlConnection,
    actor: &Profile,
    role_ids: &[u64],
) -> Result<(), OpsError> {
    for rid in role_ids {
        let codes = repo::permission_codes_by_id_tx(tx, *rid)
            .await
            .map_err(OpsError::from)?;
        if codes.is_empty() {
            return Err(OpsError::bad_input("角色不存在或没有任何权限"));
        }
        if codes.iter().any(|c| !actor.permissions.contains(c)) {
            return Err(OpsError::forbidden("不能授予自己未拥有的权限"));
        }
    }
    Ok(())
}

async fn validate_grant_tx(
    _tx: &mut MySqlConnection,
    actor: &Profile,
    permissions: &[String],
) -> Result<(), OpsError> {
    for p in permissions {
        if !actor.permissions.contains(p) {
            return Err(OpsError::bad_input(
                "角色无效：只能从自己已持有的权限中选择",
            ));
        }
    }
    Ok(())
}

fn admin_snapshot(a: &crate::AdminAccount) -> Value {
    json!({
        "id": a.id, "username": a.username, "display_name": a.display_name,
        "role_id": a.role_id, "status": a.status, "mfa_enabled": a.mfa_enabled,
    })
}

fn duplicate_or(e: sqlx::Error, msg: &str) -> OpsError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        return OpsError::conflict(msg);
    }
    OpsError::Storage(e.into())
}
