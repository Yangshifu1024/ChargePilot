//! admin_user / role / permission / role_permission 表访问（蓝图第五章 repo 层）。

use crate::OpsError;
use chargepilot_domain::operations::AdminUserStatus;
use sqlx::{MySqlConnection, MySqlPool};

/// 账号列表行（字段与契约一致）。
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
    pub status: AdminUserStatus,
    pub mfa_enabled: bool,
    pub last_login_at: Option<String>,
    pub locked_until: Option<String>,
    pub failed_login_count: u32,
}

/// 角色列表行。
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

/// 权限行。
#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct PermissionRow {
    pub id: u64,
    pub code: String,
    pub name: String,
    pub module: String,
    pub description: String,
}

/// 账号总数（关键词/状态过滤）。
pub async fn admin_count(pool: &MySqlPool, kw: &str, status: Option<&str>) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM `admin_user` u WHERE u.`deleted_at` IS NULL \
         AND (u.`username` LIKE ? ESCAPE '!' OR u.`display_name` LIKE ? ESCAPE '!') \
         AND (? IS NULL OR u.`status` = ?)",
    )
    .bind(kw)
    .bind(kw)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?)
}

/// 账号分页行（id 倒序）。
#[allow(clippy::too_many_arguments)]
pub async fn admin_page(
    pool: &MySqlPool,
    kw: &str,
    status: Option<&str>,
    page_size: i64,
    offset: i64,
) -> anyhow::Result<Vec<AdminUserRow>> {
    Ok(sqlx::query_as::<_, AdminUserRow>(
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
    .bind(kw)
    .bind(kw)
    .bind(status)
    .bind(status)
    .bind(page_size)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// 创建账号；用户名唯一键冲突原样上抛（调用方按业务冲突分类）。
pub async fn insert_admin_tx(
    tx: &mut MySqlConnection,
    id: u64,
    username: &str,
    display_name: &Option<String>,
    password_hash: String,
    role_id: u64,
    phone: &Option<String>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `display_name`, `password_hash`, `role_id`, `phone`) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(username)
    .bind(display_name)
    .bind(password_hash)
    .bind(role_id)
    .bind(phone)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 角色/状态变更（递增 auth_version 撤销旧会话）。
pub async fn set_admin_role_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query(
        "UPDATE `admin_user` SET `role_id` = ?, `auth_version` = `auth_version` + 1 WHERE `id` = ?",
    )
    .bind(role_id)
    .bind(id)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// 状态变更（递增 auth_version）。
pub async fn set_admin_status_tx(
    tx: &mut MySqlConnection,
    status: AdminUserStatus,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query(
        "UPDATE `admin_user` SET `status` = ?, `auth_version` = `auth_version` + 1 WHERE `id` = ?",
    )
    .bind(status)
    .bind(id)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// 资料字段更新（不撤销会话）。
pub async fn set_admin_display_name_tx(
    tx: &mut MySqlConnection,
    value: &str,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query("UPDATE `admin_user` SET `display_name` = ? WHERE `id` = ?")
        .bind(value)
        .bind(id)
        .execute(tx)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// 手机号更新。
pub async fn set_admin_phone_tx(
    tx: &mut MySqlConnection,
    value: &str,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query("UPDATE `admin_user` SET `phone` = ? WHERE `id` = ?")
        .bind(value)
        .bind(id)
        .execute(tx)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// 邮箱更新。
pub async fn set_admin_email_tx(
    tx: &mut MySqlConnection,
    value: &str,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query("UPDATE `admin_user` SET `email` = ? WHERE `id` = ?")
        .bind(value)
        .bind(id)
        .execute(tx)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// 软删账号（disabled + auth_version 递增）。
pub async fn soft_delete_admin_tx(
    tx: &mut MySqlConnection,
    deleted_by: u64,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query(
        "UPDATE `admin_user` SET `deleted_at` = UTC_TIMESTAMP(3), `deleted_by` = ?, `status` = ?, \
         `auth_version` = `auth_version` + 1 WHERE `id` = ?",
    )
    .bind(deleted_by)
    .bind(AdminUserStatus::Disabled)
    .bind(id)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// 解锁（恢复 active、清零失败计数、撤销旧会话）。
pub async fn unlock_admin_tx(tx: &mut MySqlConnection, id: u64) -> Result<(), OpsError> {
    sqlx::query(
        "UPDATE `admin_user` SET `status` = ?, `failed_login_count` = 0, `locked_until` = NULL, \
         `auth_version` = `auth_version` + 1 WHERE `id` = ?",
    )
    .bind(AdminUserStatus::Active)
    .bind(id)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// 重置口令（仅原状态 locked 恢复 active）。
pub async fn reset_password_tx(
    tx: &mut MySqlConnection,
    password_hash: String,
    id: u64,
) -> Result<(), OpsError> {
    sqlx::query(
        "UPDATE `admin_user` SET `password_hash` = ?, `auth_version` = `auth_version` + 1, \
         `failed_login_count` = 0, `locked_until` = NULL, \
         `status` = IF(`status` = ?, ?, `status`) WHERE `id` = ?",
    )
    .bind(AdminUserStatus::Locked)
    .bind(AdminUserStatus::Active)
    .bind(password_hash)
    .bind(id)
    .execute(tx)
    .await
    .map_err(OpsError::from)?;
    Ok(())
}

/// 角色全量行（含账号计数子查询，id 正序）。
pub async fn role_rows(pool: &MySqlPool) -> anyhow::Result<Vec<RoleRow>> {
    Ok(sqlx::query_as::<_, RoleRow>(
        "SELECT r.`id`, r.`code`, r.`name`, r.`description`, r.`is_builtin`, NULL AS `permissions`, \
                (SELECT COUNT(*) FROM `admin_user` u WHERE u.`role_id` = r.`id` AND u.`deleted_at` IS NULL) AS `user_count` \
         FROM `role` r WHERE r.`deleted_at` IS NULL ORDER BY r.`id`",
    )
    .fetch_all(pool)
    .await?)
}

/// 权限全量行（module, code 序）。
pub async fn permission_rows(pool: &MySqlPool) -> Result<Vec<PermissionRow>, OpsError> {
    sqlx::query_as::<_, PermissionRow>(
        "SELECT `id`, `code`, `name`, `module`, COALESCE(`description`, '') AS `description` \
         FROM `permission` ORDER BY `module`, `code`",
    )
    .fetch_all(pool)
    .await
    .map_err(OpsError::from)
}

/// 创建角色；编码唯一键冲突原样上抛。
pub async fn insert_role_tx(
    tx: &mut MySqlConnection,
    id: u64,
    code: &str,
    name: &str,
    description: &Option<String>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO `role` (`id`, `code`, `name`, `description`, `is_builtin`) VALUES (?, ?, ?, ?, 0)")
        .bind(id)
        .bind(code)
        .bind(name)
        .bind(description)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 锁角色行读 (id, name, is_builtin)（FOR UPDATE）。
pub async fn lock_role_name_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
) -> Result<Option<(u64, String, i64)>, OpsError> {
    sqlx::query_as::<_, (u64, String, i64)>(
        "SELECT `id`, `name`, `is_builtin` FROM `role` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_optional(tx)
    .await
    .map_err(OpsError::from)
}

/// 更新角色名称与描述。
pub async fn update_role_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
    name: &str,
    description: &Option<String>,
) -> Result<(), OpsError> {
    sqlx::query("UPDATE `role` SET `name` = ?, `description` = ? WHERE `id` = ?")
        .bind(name)
        .bind(description)
        .bind(role_id)
        .execute(tx)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// 锁角色行读 (id, is_builtin, code)（FOR UPDATE，删除判定用）。
pub async fn lock_role_delete_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
) -> Result<Option<(u64, i64, Option<String>)>, OpsError> {
    sqlx::query_as::<_, (u64, i64, Option<String>)>(
        "SELECT `id`, `is_builtin`, `code` FROM `role` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_optional(tx)
    .await
    .map_err(OpsError::from)
}

/// 角色下在用账号计数（FOR UPDATE，删除保护）。
pub async fn role_user_count_tx(tx: &mut MySqlConnection, role_id: u64) -> Result<i64, OpsError> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `admin_user` WHERE `role_id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(role_id)
    .fetch_one(tx)
    .await
    .map_err(OpsError::from)
}

/// 软删角色。
pub async fn soft_delete_role_tx(tx: &mut MySqlConnection, role_id: u64) -> Result<(), OpsError> {
    sqlx::query("UPDATE `role` SET `deleted_at` = UTC_TIMESTAMP(3) WHERE `id` = ?")
        .bind(role_id)
        .execute(tx)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// 清空角色权限关联。
pub async fn delete_role_permissions_tx(
    tx: &mut MySqlConnection,
    role_id: u64,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 重建角色权限关联（逐码插入）。
pub async fn replace_role_permissions_tx(
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

/// 角色 ID 取号（max+1，管理低频足够）。
pub async fn next_role_id_tx(tx: &mut MySqlConnection) -> anyhow::Result<u64> {
    let max: Option<u64> = sqlx::query_scalar("SELECT COALESCE(MAX(`id`), 0) FROM `role`")
        .fetch_one(&mut *tx)
        .await?;
    Ok(max.unwrap_or(0) + 1)
}
