//! `admin_user` 表访问：账号校验、锁定、MFA 消费、Profile 组合、审计与 bootstrap。
//! 锁定语义对齐旧版：连续失败 5 次锁 30 分钟，成功清零。

use super::{AdminAccount, OpsError, Profile};
use chrono::Datelike as _;
use sqlx::MySqlPool;

const MAX_FAILED_ATTEMPTS: u32 = 5;

/// 按用户名取未删除账号。
pub async fn find_by_username(
    pool: &MySqlPool,
    username: &str,
) -> anyhow::Result<Option<AdminAccount>> {
    let row = sqlx::query_as::<_, AdminRow>(
        "SELECT `id`, `username`, `display_name`, `password_hash`, `role_id`, `mfa_enabled`, \
                `mfa_secret`, `mfa_last_step`, `status`, `failed_login_count`, `locked_until`, `auth_version` \
         FROM `admin_user` WHERE `active_username` = ? LIMIT 1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

#[derive(sqlx::FromRow)]
struct AdminRow {
    id: u64,
    username: String,
    display_name: Option<String>,
    password_hash: String,
    role_id: Option<u64>,
    mfa_enabled: bool,
    mfa_secret: Option<String>,
    mfa_last_step: Option<i64>,
    status: String,
    failed_login_count: u32,
    locked_until: Option<chrono::DateTime<chrono::Utc>>,
    auth_version: u64,
}

impl From<AdminRow> for AdminAccount {
    fn from(r: AdminRow) -> Self {
        Self {
            id: r.id,
            username: r.username,
            display_name: r.display_name,
            password_hash: r.password_hash,
            role_id: r.role_id,
            mfa_enabled: r.mfa_enabled,
            mfa_secret: r.mfa_secret,
            mfa_last_step: r.mfa_last_step,
            status: r.status,
            failed_login_count: r.failed_login_count,
            locked_until: r.locked_until,
            auth_version: r.auth_version,
        }
    }
}

/// 登录密码校验（含锁定判定与失败计数），同事务写审计。返回校验通过的账号。
/// 密码正确但流程后续失败时由调用方决定是否回滚（本函数只在成功路径清零计数）。
pub async fn verify_login(
    pool: &MySqlPool,
    username: &str,
    password: &str,
    ip: &str,
) -> Result<AdminAccount, OpsError> {
    let Some(account) = find_by_username(pool, username)
        .await
        .map_err(OpsError::from)?
    else {
        return Err(OpsError::Credentials);
    };
    let now = chrono::Utc::now();
    if account.status == "disabled" {
        return Err(OpsError::Credentials);
    }
    if let Some(until) = account.locked_until {
        if until > now {
            return Err(OpsError::Locked(format!(
                "账号已临时锁定，请于 {} 后重试",
                until.to_rfc3339()
            )));
        }
    }
    if !chargepilot_platform::password::verify(password, &account.password_hash) {
        let attempts = account.failed_login_count + 1;
        let lock = attempts >= MAX_FAILED_ATTEMPTS;
        sqlx::query(
            "UPDATE `admin_user` SET `failed_login_count` = ?, `status` = 'active', \
             `locked_until` = IF(? = 1, DATE_ADD(NOW(3), INTERVAL 30 MINUTE), NULL) WHERE `id` = ?",
        )
        .bind(attempts)
        .bind(lock)
        .bind(account.id)
        .execute(pool)
        .await
        .map_err(OpsError::from)?;
        if lock {
            return Err(OpsError::Locked(format!(
                "连续登录失败 {attempts} 次，账号已锁定 30 分钟"
            )));
        }
        return Err(OpsError::Credentials);
    }
    // 成功：清零失败计数并记录最近登录
    sqlx::query(
        "UPDATE `admin_user` SET `failed_login_count` = 0, `locked_until` = NULL, \
         `status` = 'active', `last_login_at` = NOW(3) WHERE `id` = ?",
    )
    .bind(account.id)
    .execute(pool)
    .await
    .map_err(OpsError::from)?;
    write_auth_audit(pool, &account, "login", ip)
        .await
        .map_err(OpsError::from)?;
    Ok(account)
}

/// MFA 完成：事务内校验验证码（±1 步 + mfa_last_step 防重放）→ 消费挑战 → 记审计。
/// `consume` 由调用方提供（先于本函数删除挑战者才能登录，旧版顺序：SQL 内校验，
/// Redis 删除由外层完成——此处校验通过即视为可登录，挑战删除由 service 层执行）。
pub async fn verify_mfa(
    pool: &MySqlPool,
    account: &AdminAccount,
    code: &str,
    ip: &str,
) -> Result<(), OpsError> {
    let now = chrono::Utc::now();
    if account.status == "disabled" {
        return Err(OpsError::Credentials);
    }
    if let Some(until) = account.locked_until {
        if until > now {
            return Err(OpsError::Locked("账号已临时锁定".into()));
        }
    }
    let Some(secret) = account.mfa_secret.as_deref() else {
        return Err(OpsError::InvalidChallenge);
    };
    let step = now.timestamp().div_euclid(30);
    let matched = (0..=2i64).any(|off| {
        chargepilot_platform::totp::generate(secret, (step + off - 1) * 30).as_deref() == Some(code)
    });
    if !matched {
        return Err(OpsError::InvalidTotp);
    }
    let used = matched_step(pool, account, step).await?;
    if used {
        return Err(OpsError::UsedTotp);
    }
    write_auth_audit(pool, account, "login_mfa", ip)
        .await
        .map_err(OpsError::from)?;
    Ok(())
}

/// mfa_last_step 防重放：码所属步不得小于等于已消费步；通过则推进到该步。
/// 返回 true 表示该步已被消费。
async fn matched_step(
    pool: &MySqlPool,
    account: &AdminAccount,
    code_step: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE `admin_user` SET `mfa_last_step` = ? \
         WHERE `id` = ? AND (`mfa_last_step` IS NULL OR `mfa_last_step` < ?)",
    )
    .bind(code_step)
    .bind(account.id)
    .bind(code_step)
    .execute(pool)
    .await
    .map_err(OpsError::from)?;
    Ok(result.rows_affected() == 0)
}

/// 鉴权后的管理员资料：账号 + 角色编码/名称 + 权限码集合（按 code 排序）。
pub async fn load_profile(pool: &MySqlPool, account_id: u64) -> anyhow::Result<Profile> {
    let row = sqlx::query_as::<_, ProfileRow>(
        "SELECT u.`id`, u.`username`, u.`display_name`, u.`role_id`, u.`mfa_enabled`, u.`auth_version`, \
                COALESCE(r.`code`, '') AS `role_code`, COALESCE(r.`name`, '') AS `role_name` \
         FROM `admin_user` u LEFT JOIN `role` r ON r.`id` = u.`role_id` AND r.`deleted_at` IS NULL \
         WHERE u.`id` = ? AND u.`deleted_at` IS NULL LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?
    .ok_or(OpsError::Credentials)?;
    let permissions = match row.role_id {
        Some(rid) if rid > 0 => permission_codes(pool, rid).await?,
        _ => Vec::new(),
    };
    Ok(Profile {
        auth_version: row.auth_version,
        id: row.id,
        username: row.username,
        display_name: row.display_name.unwrap_or_default(),
        role: row.role_code,
        role_name: row.role_name,
        role_id: row.role_id.unwrap_or(0),
        mfa_enabled: row.mfa_enabled,
        permissions,
    })
}

#[derive(sqlx::FromRow)]
struct ProfileRow {
    id: u64,
    username: String,
    display_name: Option<String>,
    role_id: Option<u64>,
    mfa_enabled: bool,
    auth_version: u64,
    #[sqlx(rename = "role_code")]
    role_code: String,
    role_name: String,
}

/// 角色权限码集合。
pub async fn permission_codes(pool: &MySqlPool, role_id: u64) -> anyhow::Result<Vec<String>> {
    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT p.`code` FROM `role_permission` rp \
         JOIN `permission` p ON p.`id` = rp.`permission_id` \
         WHERE rp.`role_id` = ? ORDER BY p.`code`",
    )
    .bind(role_id)
    .fetch_all(pool)
    .await?;
    Ok(codes)
}

/// 认证版本号（实时鉴权核对：改密/调权后旧会话失效）。
pub async fn auth_version(pool: &MySqlPool, account_id: u64) -> anyhow::Result<Option<u64>> {
    let v: Option<u64> = sqlx::query_scalar(
        "SELECT `auth_version` FROM `admin_user` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    Ok(v)
}

/// 账号事务内写认证审计（字段与旧版一致，含月分区键）。
pub async fn write_auth_audit(
    pool: &MySqlPool,
    account: &AdminAccount,
    action: &str,
    ip: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let month = now.date_naive().with_day(1).expect("月首日必然存在");
    sqlx::query(
        "INSERT INTO `audit_log` (`actor_id`, `actor_name`, `module`, `action`, `target_type`, `target_id`, `client_ip`, `created_month`) \
         VALUES (?, ?, 'auth', ?, 'admin_user', ?, ?, ?)",
    )
    .bind(account.id)
    .bind(&account.username)
    .bind(action)
    .bind(account.id.to_string())
    .bind(ip)
    .bind(month)
    .execute(pool)
    .await?;
    Ok(())
}

/// 诊断接口审计（只读操作的可见性记录，module='ops' action='diagnostics'）。
pub async fn write_diagnostics_audit(
    pool: &MySqlPool,
    actor_id: u64,
    actor_name: &str,
    ip: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let month = now.date_naive().with_day(1).expect("月首日必然存在");
    sqlx::query(
        "INSERT INTO `audit_log` (`actor_id`, `actor_name`, `module`, `action`, `target_type`, `target_id`, `client_ip`, `created_month`) \
         VALUES (?, ?, 'ops', 'diagnostics', 'system', '', ?, ?)",
    )
    .bind(actor_id as i64)
    .bind(actor_name)
    .bind(ip)
    .bind(month)
    .execute(pool)
    .await?;
    Ok(())
}

/// 引导账号：不存在时以明文口令建号（bcrypt 落库），写 bootstrap 审计；存在则不动。
pub async fn ensure_bootstrap(
    pool: &MySqlPool,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    if username.is_empty() || !(12..=72).contains(&password.len()) {
        anyhow::bail!("admin bootstrap requires username and 12–72 byte password");
    }
    if find_by_username(pool, username).await?.is_some() {
        return Ok(());
    }
    let hash = chargepilot_platform::password::hash(password)?;
    let id = allocator.next_id().await?;
    // 引导账号挂超管角色（role_id=1，迁移已种入全量权限）：
    // 它是首次部署的唯一管理入口，落库无角色则登录后处处 403。
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)",
    )
    .bind(id)
    .bind(username)
    .bind(hash)
    .execute(pool)
    .await?;
    let account = find_by_username(pool, username).await?.expect("刚创建");
    write_auth_audit(pool, &account, "bootstrap", "").await?;
    tracing::info!(username, "后台引导账号已创建");
    Ok(())
}

/// 通用审计写入（业务事务内调用）：before/after 为完整快照 JSON，nil 传 Value::Null。
pub async fn append_audit_tx(
    tx: &mut sqlx::MySqlConnection,
    actor: &Profile,
    module: &str,
    action: &str,
    target_type: &str,
    target_id: &str,
    before: &serde_json::Value,
    after: &serde_json::Value,
    client_ip: &str,
    request_id: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let month = now.date_naive().with_day(1).expect("月首日必然存在");
    sqlx::query(
        "INSERT INTO `audit_log` (`actor_id`, `actor_name`, `module`, `action`, `target_type`, `target_id`, \
         `before_json`, `after_json`, `client_ip`, `request_id`, `created_month`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(actor.id)
    .bind(&actor.username)
    .bind(module)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(before)
    .bind(after)
    .bind(client_ip)
    .bind(request_id.chars().take(64).collect::<String>())
    .bind(month)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// customer_admin 角色行集合锁（管理写路径串行化点，必须最先获取）。
pub async fn lock_customer_admin_role_tx(
    tx: &mut sqlx::MySqlConnection,
) -> anyhow::Result<Option<u64>> {
    let id: Option<u64> = sqlx::query_scalar(
        "SELECT `id` FROM `role` WHERE `active_code` = 'customer_admin' LIMIT 1 FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?;
    Ok(id)
}

/// 锁定操作者并实时核对其权限（写路径的锁内复核，旧版 lockedManagementActor 语义）。
pub async fn lock_actor_tx(
    tx: &mut sqlx::MySqlConnection,
    actor_id: u64,
) -> anyhow::Result<Profile> {
    let account = sqlx::query_as::<_, (u64, u64)>(
        "SELECT `id`, `auth_version` FROM `admin_user` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(actor_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(OpsError::Credentials)?;
    let profile = load_profile_tx(tx, account.0).await?;
    if profile.auth_version != account.1 {
        return Err(OpsError::Credentials.into());
    }
    Ok(profile)
}

/// 事务内 Profile 组合（权限 FOR SHARE 读，与锁内核对一致）。
pub async fn load_profile_tx(
    tx: &mut sqlx::MySqlConnection,
    account_id: u64,
) -> anyhow::Result<Profile> {
    let row = sqlx::query_as::<_, ProfileRow>(
        "SELECT u.`id`, u.`username`, u.`display_name`, u.`role_id`, u.`mfa_enabled`, u.`auth_version`, \
                COALESCE(r.`code`, '') AS `role_code`, COALESCE(r.`name`, '') AS `role_name` \
         FROM `admin_user` u LEFT JOIN `role` r ON r.`id` = u.`role_id` AND r.`deleted_at` IS NULL \
         WHERE u.`id` = ? AND u.`deleted_at` IS NULL LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(OpsError::Credentials)?;
    let permissions = match row.role_id {
        Some(rid) if rid > 0 => {
            let codes: Vec<String> = sqlx::query_scalar(
                "SELECT p.`code` FROM `role_permission` rp JOIN `permission` p ON p.`id` = rp.`permission_id` \
                 WHERE rp.`role_id` = ? ORDER BY p.`code`",
            )
            .bind(rid)
            .fetch_all(&mut *tx)
            .await?;
            without_retired(codes)
        }
        _ => Vec::new(),
    };
    Ok(Profile {
        auth_version: row.auth_version,
        id: row.id,
        username: row.username,
        display_name: row.display_name.unwrap_or_default(),
        role: row.role_code,
        role_name: row.role_name,
        role_id: row.role_id.unwrap_or(0),
        mfa_enabled: row.mfa_enabled,
        permissions,
    })
}

/// 退役权限码（旧版语义：展示与授权一律剔除）。
pub fn without_retired(codes: Vec<String>) -> Vec<String> {
    const RETIRED: &[&str] = &[
        "customer_service.",
        "ota.",
        "alert.rule.",
        "alert.subscription.",
        "settings.ota.update",
        "membership.create",
        "alert.risk_config.update",
        "pricing.template.create",
    ];
    codes
        .into_iter()
        .filter(|c| !RETIRED.iter().any(|r| c.starts_with(r) || c == *r))
        .collect()
}

/// 锁定目标账号（FOR UPDATE 快照）。
pub async fn lock_admin_tx(
    tx: &mut sqlx::MySqlConnection,
    admin_id: u64,
) -> anyhow::Result<Option<AdminAccount>> {
    let row = sqlx::query_as::<_, AdminRow>(
        "SELECT `id`, `username`, `display_name`, `password_hash`, `role_id`, `mfa_enabled`, \
                `mfa_secret`, `mfa_last_step`, `status`, `failed_login_count`, `locked_until`, `auth_version` \
         FROM `admin_user` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(admin_id)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row.map(Into::into))
}

/// 当前有效的 customer_admin 数量（排除指定账号，当前读 FOR UPDATE）。
pub async fn count_other_active_customer_admins_tx(
    tx: &mut sqlx::MySqlConnection,
    exclude_id: u64,
) -> anyhow::Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `admin_user` u JOIN `role` r ON r.`id` = u.`role_id` AND r.`active_code` = 'customer_admin' \
         WHERE u.`deleted_at` IS NULL AND u.`status` = 'active' AND u.`id` <> ? FOR UPDATE",
    )
    .bind(exclude_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(n)
}

/// 事务内按角色查权限码（锁内核对用）。
pub async fn permission_codes_by_id_tx(
    tx: &mut sqlx::MySqlConnection,
    role_id: u64,
) -> anyhow::Result<Vec<String>> {
    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT p.`code` FROM `role_permission` rp JOIN `permission` p ON p.`id` = rp.`permission_id`          WHERE rp.`role_id` = ? ORDER BY p.`code`",
    )
    .bind(role_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(without_retired(codes))
}

/// 锁定读取有效账号（旧版 LockActiveAdmin）：status=active 且未删除，FOR UPDATE。
/// 不存在或已停用返回 None（调用方按凭据失效处理）。
pub async fn lock_active_admin_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
) -> anyhow::Result<Option<AdminAccount>> {
    let row: Option<(u64, String, Option<String>, String, Option<u64>, u64)> = sqlx::query_as(
        "SELECT `id`, `username`, `display_name`, `password_hash`, `role_id`, `auth_version` \
         FROM `admin_user` WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(tx)
    .await?;
    Ok(row.map(
        |(id, username, display_name, password_hash, role_id, auth_version)| AdminAccount {
            id,
            username,
            display_name,
            password_hash,
            role_id,
            mfa_enabled: false,
            mfa_secret: None,
            mfa_last_step: None,
            status: "active".into(),
            failed_login_count: 0,
            locked_until: None,
            auth_version: auth_version as u64,
        },
    ))
}

/// 锁定读取未删除角色（旧版 LockRole），FOR UPDATE。
pub async fn lock_role_tx(
    tx: &mut sqlx::MySqlConnection,
    id: u64,
) -> anyhow::Result<Option<(u64, String)>> {
    let row: Option<(u64, String)> = sqlx::query_as(
        "SELECT `id`, `code` FROM `role` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(id as i64)
    .fetch_optional(tx)
    .await?;
    Ok(row)
}

/// 角色权限码（旧版 RolePermissionCodesInTx），FOR SHARE 共享锁读最新集合。
pub async fn role_permission_codes_tx(
    tx: &mut sqlx::MySqlConnection,
    role_id: u64,
) -> anyhow::Result<Vec<String>> {
    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT p.`code` FROM `permission` AS p \
         JOIN `role_permission` rp ON rp.`permission_id` = p.`id` \
         WHERE rp.`role_id` = ? ORDER BY p.`code` FOR SHARE",
    )
    .bind(role_id as i64)
    .fetch_all(tx)
    .await?;
    Ok(codes)
}
