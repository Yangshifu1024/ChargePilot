//! 后台账号与角色管理：锁顺序固定为「customer_admin 角色行 → 操作者 → 目标行」，
//! 最后管理员保护与越权校验在锁内复核（旧版语义）。审计与业务同事务提交。

use super::repo;
use super::{OpsError, Profile};
use serde_json::{Value, json};
use sqlx::{MySqlConnection, MySqlPool};

const MODULE_ADMIN_USER: &str = "admin_user";
const MODULE_ROLE: &str = "role";

/// 分页参数（对齐旧版：page≥1、page_size 1–100 默认 20）。
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub page: u32,
    pub page_size: u32,
}

impl Page {
    pub fn sanitized(page: i64, page_size: i64) -> anyhow::Result<Self> {
        if !(1..=1_000_000).contains(&page) {
            anyhow::bail!("分页参数无效");
        }
        if !(1..=100).contains(&page_size) {
            anyhow::bail!("分页参数无效：page_size 为 1–100");
        }
        Ok(Self {
            page: page as u32,
            page_size: page_size as u32,
        })
    }

    fn offset(&self) -> i64 {
        ((self.page - 1) as i64) * (self.page_size as i64)
    }
}

// ---------------------------------------------------------------------------
// 账号管理
// ---------------------------------------------------------------------------

/// 账号列表行（字段与旧版一致）。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct AdminUserRow {
    pub id: u64,
    pub username: String,
    pub display_name: Option<String>,
    pub role_id: Option<u64>,
    pub role_code: Option<String>,
    pub role_name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub status: String,
    pub mfa_enabled: bool,
    pub last_login_at: Option<String>,
    pub locked_until: Option<String>,
    pub failed_login_count: u32,
}

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
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `admin_user` u WHERE u.`deleted_at` IS NULL \
         AND (u.`username` LIKE ? ESCAPE '!' OR u.`display_name` LIKE ? ESCAPE '!') \
         AND (? IS NULL OR u.`status` = ?)",
    )
    .bind(&kw)
    .bind(&kw)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as::<_, AdminUserRow>(
        "SELECT u.`id`, u.`username`, u.`display_name`, u.`role_id`, r.`code` AS `role_code`, r.`name` AS `role_name`, \
                u.`phone`, u.`email`, u.`status`, u.`mfa_enabled`, \
                DATE_FORMAT(u.`last_login_at`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `last_login_at`, \
                DATE_FORMAT(u.`locked_until`, '%Y-%m-%dT%H:%i:%s.%f+00:00') AS `locked_until`, \
                u.`failed_login_count` \
         FROM `admin_user` u LEFT JOIN `role` r ON r.`id` = u.`role_id` \
         WHERE u.`deleted_at` IS NULL \
           AND (u.`username` LIKE ? ESCAPE '!' OR u.`display_name` LIKE ? ESCAPE '!') \
           AND (? IS NULL OR u.`status` = ?) \
         ORDER BY u.`id` DESC LIMIT ? OFFSET ?",
    )
    .bind(&kw)
    .bind(&kw)
    .bind(status)
    .bind(status)
    .bind(page.page_size as i64)
    .bind(page.offset())
    .fetch_all(pool)
    .await?;
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
        || !(12..=72).contains(&input.password.len())
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
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `display_name`, `password_hash`, `role_id`, `phone`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id)
    .bind(&input.username)
    .bind(&input.display_name)
    .bind(hash)
    .bind(input.role_id)
    .bind(&input.phone)
    .execute(&mut *tx)
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
    if let Some(s) = &input.status
        && s != "active"
        && s != "disabled"
    {
        return Err(OpsError::bad_input("账号状态无效"));
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
    let status_changed = input.status.as_deref().is_some_and(|s| s != target.status);
    if target.id == actor.id && (status_changed || role_changed) {
        return Err(OpsError::conflict("不能修改自己的账号状态或角色"));
    }
    let mut sessions_revoked = false;
    if let Some(rid) = input.role_id {
        sqlx::query("UPDATE `admin_user` SET `role_id` = ?, `auth_version` = `auth_version` + 1 WHERE `id` = ?")
            .bind(rid).bind(target.id).execute(&mut *tx).await.map_err(OpsError::from)?;
        sessions_revoked = true;
    }
    if let Some(s) = &input.status {
        sqlx::query("UPDATE `admin_user` SET `status` = ?, `auth_version` = `auth_version` + 1 WHERE `id` = ?")
            .bind(s).bind(target.id).execute(&mut *tx).await.map_err(OpsError::from)?;
        sessions_revoked = true;
    }
    if let Some(d) = &input.display_name {
        sqlx::query("UPDATE `admin_user` SET `display_name` = ? WHERE `id` = ?")
            .bind(d)
            .bind(target.id)
            .execute(&mut *tx)
            .await
            .map_err(OpsError::from)?;
    }
    if let Some(p) = &input.phone {
        sqlx::query("UPDATE `admin_user` SET `phone` = ? WHERE `id` = ?")
            .bind(p)
            .bind(target.id)
            .execute(&mut *tx)
            .await
            .map_err(OpsError::from)?;
    }
    if let Some(e) = &input.email {
        sqlx::query("UPDATE `admin_user` SET `email` = ? WHERE `id` = ?")
            .bind(e)
            .bind(target.id)
            .execute(&mut *tx)
            .await
            .map_err(OpsError::from)?;
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
    sqlx::query(
        "UPDATE `admin_user` SET `deleted_at` = UTC_TIMESTAMP(3), `deleted_by` = ?, `status` = 'disabled', \
         `auth_version` = `auth_version` + 1 WHERE `id` = ?",
    )
    .bind(actor.id)
    .bind(target.id)
    .execute(&mut *tx)
    .await
    .map_err(OpsError::from)?;
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
    if target.status != "active" {
        sqlx::query(
            "UPDATE `admin_user` SET `status` = 'active', `failed_login_count` = 0, `locked_until` = NULL, \
             `auth_version` = `auth_version` + 1 WHERE `id` = ?",
        )
        .bind(target.id)
        .execute(&mut *tx)
        .await
        .map_err(OpsError::from)?;
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
    if !(12..=72).contains(&new_password.len()) {
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
    // 仅原状态为 locked 时恢复 active（停用账号重置密码后仍停用，旧版语义）
    sqlx::query(
        "UPDATE `admin_user` SET `password_hash` = ?, `auth_version` = `auth_version` + 1, \
         `failed_login_count` = 0, `locked_until` = NULL, \
         `status` = IF(`status` = 'locked', 'active', `status`) WHERE `id` = ?",
    )
    .bind(hash)
    .bind(target.id)
    .execute(&mut *tx)
    .await
    .map_err(OpsError::from)?;
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

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct RoleRow {
    pub id: u64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_builtin: bool,
    pub permissions: Option<serde_json::Value>,
    pub user_count: i64,
}

pub async fn list_roles(pool: &MySqlPool) -> Result<Vec<RoleRow>, OpsError> {
    let mut roles = sqlx::query_as::<_, RoleRow>(
        "SELECT r.`id`, r.`code`, r.`name`, r.`description`, r.`is_builtin`, NULL AS `permissions`, \
                (SELECT COUNT(*) FROM `admin_user` u WHERE u.`role_id` = r.`id` AND u.`deleted_at` IS NULL) AS `user_count` \
         FROM `role` r WHERE r.`deleted_at` IS NULL ORDER BY r.`id`",
    )
    .fetch_all(pool)
    .await?;
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

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct PermissionRow {
    pub id: u64,
    pub code: String,
    pub name: String,
    pub module: String,
    pub description: String,
}

pub async fn list_permissions(pool: &MySqlPool) -> Result<Vec<PermissionRow>, OpsError> {
    sqlx::query_as::<_, PermissionRow>(
        "SELECT `id`, `code`, `name`, `module`, COALESCE(`description`, '') AS `description` \
         FROM `permission` ORDER BY `module`, `code`",
    )
    .fetch_all(pool)
    .await
    .map_err(OpsError::from)
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
    let id = next_role_id_tx(&mut tx).await?;
    sqlx::query("INSERT INTO `role` (`id`, `code`, `name`, `description`, `is_builtin`) VALUES (?, ?, ?, ?, 0)")
        .bind(id)
        .bind(&input.code)
        .bind(input.name.trim())
        .bind(&input.description)
        .execute(&mut *tx)
        .await
        .map_err(|e| duplicate_or(e, "编码已存在，请使用其他编码"))?;
    replace_role_permissions_tx(&mut tx, id, &input.permissions).await?;
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
    let existing = sqlx::query_as::<_, (u64, String, i64)>(
        "SELECT `id`, `name`, `is_builtin` FROM `role` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(OpsError::from)?
    .ok_or_else(|| OpsError::not_found("角色不存在"))?;
    let before_permissions: Vec<String> = if existing.2 == 1 {
        repo::permission_codes(pool, role_id)
            .await
            .map_err(OpsError::from)?
    } else {
        Vec::new()
    };
    let permissions_locked = existing.2 == 1;
    sqlx::query("UPDATE `role` SET `name` = ?, `description` = ? WHERE `id` = ?")
        .bind(input.name.trim())
        .bind(&input.description)
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(OpsError::from)?;
    if !permissions_locked {
        validate_grant_tx(&mut tx, &actor_locked, &input.permissions).await?;
        replace_role_permissions_tx(&mut tx, role_id, &input.permissions).await?;
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
    let existing = sqlx::query_as::<_, (u64, i64, Option<String>)>(
        "SELECT `id`, `is_builtin`, `code` FROM `role` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(OpsError::from)?
    .ok_or_else(|| OpsError::not_found("角色不存在"))?;
    if existing.1 == 1 {
        return Err(OpsError::conflict(
            "内置角色不可删除：它承载运营自身的访问权限",
        ));
    }
    if actor.role_id == role_id {
        return Err(OpsError::conflict("不能删除自己正在使用的角色"));
    }
    let users: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `admin_user` WHERE `role_id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(OpsError::from)?;
    if users > 0 {
        return Err(OpsError::conflict("该角色下仍有账号，请先改派后再删除"));
    }
    sqlx::query("UPDATE `role` SET `deleted_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(OpsError::from)?;
    sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
        .bind(role_id)
        .execute(&mut *tx)
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

async fn replace_role_permissions_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
    codes: &[String],
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    for code in codes {
        let code = code.trim();
        if code.is_empty() {
            continue;
        }
        sqlx::query("INSERT INTO `role_permission` (`role_id`, `permission_id`) SELECT ?, `id` FROM `permission` WHERE `code` = ?")
            .bind(role_id)
            .bind(code)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

async fn next_role_id_tx(tx: &mut MySqlConnection) -> anyhow::Result<u64> {
    // 角色 ID 独立于号段（旧版为 AUTO_INCREMENT 语义；此处取 max+1，管理低频足够）
    let max: Option<u64> = sqlx::query_scalar("SELECT COALESCE(MAX(`id`), 0) FROM `role`")
        .fetch_one(&mut *tx)
        .await?;
    Ok(max.unwrap_or(0) + 1)
}

fn admin_snapshot(a: &super::AdminAccount) -> Value {
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
