//! admin_user 表的 MFA 事务原语与状态读取（蓝图第五章 repo 层）。

use crate::error::MfaError;
use sqlx::MySqlConnection;

/// MFA 事务内账号行（含密钥代际与认证版本）。
#[derive(sqlx::FromRow)]
pub struct MfaAccount {
    pub id: u64,
    pub username: String,
    pub password_hash: String,
    pub mfa_enabled: bool,
    pub mfa_secret: Option<String>,
    /// TOTP 密钥代际（schema 为 bigint unsigned）。
    pub mfa_secret_version: u64,
    pub mfa_last_step: Option<i64>,
    pub auth_version: u64,
}

/// 连接级隔离级别设为 READ COMMITTED（写事务统一前置）。
pub async fn set_read_committed(c: &mut MySqlConnection) -> Result<(), MfaError> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(c)
        .await?;
    Ok(())
}

/// 锁读启用账号全列（含 MFA 与认证版本）。
pub async fn lock_active_admin_full_tx(
    c: &mut MySqlConnection,
    id: u64,
) -> Result<MfaAccount, MfaError> {
    let row: Option<MfaAccount> = sqlx::query_as(
        "SELECT `id`, `username`, `password_hash`, `mfa_enabled`, `mfa_secret`, \
                `mfa_secret_version`, `mfa_last_step`, `auth_version` \
         FROM `admin_user` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(c)
    .await?;
    row.ok_or(MfaError::Credentials)
}

/// 更新 MFA 密钥与启用态：密钥代际变化时递增 mfa_secret_version 并重置
/// mfa_last_step；bump 时递增 auth_version 撤销旧会话。
/// 调用方须已持有账号行锁，changed 依据锁内快照判定。
pub async fn update_mfa_secret_tx(
    c: &mut MySqlConnection,
    id: u64,
    new_secret: Option<&str>,
    current_secret: Option<&str>,
    enabled: bool,
    bump_auth_version: bool,
) -> Result<(), MfaError> {
    let changed = current_secret != new_secret;
    sqlx::query(
        "UPDATE `admin_user` SET `mfa_secret` = ?, `mfa_enabled` = ?, \
         `mfa_secret_version` = `mfa_secret_version` + ?, \
         `mfa_last_step` = IF(? = 1, NULL, `mfa_last_step`), \
         `auth_version` = `auth_version` + ? \
         WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(new_secret)
    .bind(enabled)
    .bind(if changed { 1 } else { 0 })
    .bind(if changed { 1 } else { 0 })
    .bind(if bump_auth_version { 1 } else { 0 })
    .bind(id)
    .execute(c)
    .await?;
    Ok(())
}

/// 消费当前密钥代际的时间步：拒绝旧步或重复步；
/// 代际不匹配或 0 行更新按验证码错误处理，由调用方整体回滚。
pub async fn consume_totp_step_tx(
    c: &mut MySqlConnection,
    account: &MfaAccount,
    step: i64,
) -> Result<(), MfaError> {
    if account.mfa_last_step.is_some_and(|last| step <= last) {
        return Err(MfaError::UsedTotp);
    }
    let n = sqlx::query(
        "UPDATE `admin_user` SET `mfa_last_step` = ? \
         WHERE `id` = ? AND `mfa_secret_version` = ? AND (`mfa_last_step` IS NULL OR `mfa_last_step` < ?)",
    )
    .bind(step)
    .bind(account.id)
    .bind(account.mfa_secret_version)
    .bind(step)
    .execute(c)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(MfaError::Proof);
    }
    Ok(())
}

/// 当前管理员 MFA 启用状态；账号不存在按凭据失效处理。
pub async fn mfa_enabled(pool: &sqlx::MySqlPool, admin_id: u64) -> Result<Option<bool>, MfaError> {
    sqlx::query_scalar(
        "SELECT `mfa_enabled` FROM `admin_user` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(admin_id)
    .fetch_optional(pool)
    .await
    .map_err(MfaError::from)
}
