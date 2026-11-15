//! 管理员 MFA：本人安全设置（enrol/confirm/disable，Redis 待绑定 + 租锁 + 频次）、
//! 管理端重置（admin-users/{id}/mfa）与状态查询。
//! 语义基准：旧仓库 internal/ops/mfa.go + account_management.go 的 MFA 分支，逐条对齐。
//! 密码、密钥与验证码不进审计；登记材料只存短期缓存（5 分钟）。

use crate::Profile;
use crate::error::MfaError;
use chargepilot_platform::sessiontoken::random_token;
use serde_json::{Value, json};
use sqlx::Acquire;
use sqlx::MySqlPool;

/// 待绑定窗口 5 分钟（旧版 mfaEnrollmentTTL）。
const ENROLLMENT_TTL_SECS: u64 = 5 * 60;
/// 设置租锁 30 秒（旧版 Acquire 30s），单次操作限时 10 秒。
const LOCK_TTL_SECS: u64 = 30;
/// 一分钟窗口内允许的安全设置次数（第 6 次拒绝，旧版 n>5）。
const SETTINGS_RATE_LIMIT: i64 = 5;

// ---------------------------------------------------------------------------
// Redis IO（旧版 mfa_repo.go：pending / 租锁 / 频次）
// ---------------------------------------------------------------------------

/// 比较读取值后删除（待绑定记录和租锁共用；旧版 mfaCompareDelete）。
const COMPARE_DELETE: &str =
    "if redis.call('GET',KEYS[1]) == ARGV[1] then return redis.call('DEL',KEYS[1]) end; return 0";

/// 原子增加计数，仅首次设置六十秒窗口（旧版 authRateScript）。
const RATE_SCRIPT: &str = "local n=redis.call('INCR',KEYS[1]); if n==1 then redis.call('EXPIRE',KEYS[1],60) end; return n";

fn pending_key(id: u64) -> String {
    format!("admin:mfa-settings:pending:{id}")
}

fn lock_key(id: u64) -> String {
    format!("admin:mfa-settings:lock:{id}")
}

fn rate_key(id: u64) -> String {
    format!("admin:mfa-settings:rate:{id}")
}

async fn conn(redis_url: &str) -> Result<redis::aio::MultiplexedConnection, MfaError> {
    Ok(redis::Client::open(redis_url)?
        .get_multiplexed_tokio_connection()
        .await?)
}

/// 保存待绑定记录（JSON，TTL 5 分钟；旧版 SavePending）。
async fn save_pending(redis_url: &str, id: u64, record: &Value) -> Result<(), MfaError> {
    let mut c = conn(redis_url).await?;
    redis::cmd("SET")
        .arg(pending_key(id))
        .arg(record.to_string())
        .arg("EX")
        .arg(ENROLLMENT_TTL_SECS)
        .query_async::<()>(&mut c)
        .await?;
    Ok(())
}

/// 读取待绑定记录原文；缺失或损坏按 Pending 过期处理（旧版 Pending）。
async fn read_pending(redis_url: &str, id: u64) -> Result<(Value, String), MfaError> {
    let mut c = conn(redis_url).await?;
    let raw: Option<String> = redis::cmd("GET")
        .arg(pending_key(id))
        .query_async(&mut c)
        .await?;
    let Some(raw) = raw else {
        return Err(MfaError::Pending);
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => Ok((v, raw)),
        Err(_) => Err(MfaError::Pending),
    }
}

/// 仅在记录仍等于读取原文时删除（旧版 ConsumePending）。
async fn consume_pending(redis_url: &str, id: u64, raw: &str) -> Result<bool, MfaError> {
    let mut c = conn(redis_url).await?;
    let n: i32 = redis::Script::new(COMPARE_DELETE)
        .key(pending_key(id))
        .arg(raw)
        .invoke_async(&mut c)
        .await?;
    Ok(n == 1)
}

/// 删除待绑定记录（旧版 ClearPending）。
async fn clear_pending(redis_url: &str, id: u64) -> Result<(), MfaError> {
    let mut c = conn(redis_url).await?;
    redis::cmd("DEL")
        .arg(pending_key(id))
        .query_async::<()>(&mut c)
        .await?;
    Ok(())
}

/// 取得账号设置租锁（旧版 Acquire，SET NX EX 30；成功返回 "OK"，占用返回 nil）。
async fn acquire_lock(redis_url: &str, id: u64, owner: &str) -> Result<bool, MfaError> {
    let mut c = conn(redis_url).await?;
    let reply: Option<String> = redis::cmd("SET")
        .arg(lock_key(id))
        .arg(owner)
        .arg("EX")
        .arg(LOCK_TTL_SECS)
        .arg("NX")
        .query_async(&mut c)
        .await?;
    Ok(reply.is_some())
}

/// 只释放仍属于自己的租锁（旧版 Release）。
async fn release_lock(redis_url: &str, id: u64, owner: &str) -> Result<(), MfaError> {
    let mut c = conn(redis_url).await?;
    let _: i32 = redis::Script::new(COMPARE_DELETE)
        .key(lock_key(id))
        .arg(owner)
        .invoke_async(&mut c)
        .await?;
    Ok(())
}

/// 一分钟窗口计数（旧版 Count）。
async fn count_rate(redis_url: &str, id: u64) -> Result<i64, MfaError> {
    let mut c = conn(redis_url).await?;
    let n: i64 = redis::Script::new(RATE_SCRIPT)
        .key(rate_key(id))
        .invoke_async(&mut c)
        .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// SQL 辅助（旧版 UpdateAdminMFASecret / consumeAdminTOTPStep 语义）
// ---------------------------------------------------------------------------

/// 校验验证码并返回命中的时间步（±1 步；与 repo::verify_mfa 同一匹配规则）。
fn verify_with_step(secret: &str, code: &str, now: i64) -> Option<i64> {
    let step = now.div_euclid(30);
    (step - 1..=step + 1)
        .find(|s| chargepilot_platform::totp::generate(secret, s * 30).as_deref() == Some(code))
}

/// 校验 MFA 设置输入（旧版 MFASettingsInput.Valid）：enrol 需密码且禁带码/绑定号；
/// confirm 需 43 字符绑定号与六位数字码；disable 需密码加六位码。
fn settings_input_valid(action: &str, password: &str, code: &str, enrollment_id: &str) -> bool {
    let password_ok = !password.is_empty() && password.len() <= 72;
    let code_ok = code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit());
    match action {
        "enrol" => password_ok && code.is_empty() && enrollment_id.is_empty(),
        "confirm" => password.is_empty() && code_ok && enrollment_id.len() == 43,
        "disable" => password_ok && code_ok && enrollment_id.is_empty(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 本人安全设置（POST /api/v1/admin/auth/mfa-settings）
// ---------------------------------------------------------------------------

/// 本人 MFA 设置（旧版 MFAService.Apply）：频次 → 租锁 → 事务（锁账号 → 核对会话
/// 认证版本 → 动作分支）。响应形状与旧版 MFASettingsReply 一致（omitempty 字段缺省省略）。
#[allow(clippy::too_many_arguments)]
pub async fn apply_settings(
    pool: &MySqlPool,
    redis_url: &str,
    profile: &Profile,
    sid: &str,
    action: &str,
    password: &str,
    code: &str,
    enrollment_id: &str,
    ip: &str,
) -> Result<Value, MfaError> {
    let code = code.trim();
    if !settings_input_valid(action, password, code, enrollment_id) {
        return Err(MfaError::Input);
    }
    if sid.is_empty() {
        return Err(MfaError::Credentials);
    }
    let n = count_rate(redis_url, profile.id).await?;
    if n > SETTINGS_RATE_LIMIT {
        return Err(MfaError::Rate);
    }
    let owner = random_token(16);
    if !acquire_lock(redis_url, profile.id, &owner).await? {
        return Err(MfaError::Busy);
    }
    let result = apply_locked(
        pool,
        redis_url,
        profile,
        sid,
        action,
        password,
        code,
        enrollment_id,
        ip,
    )
    .await;
    // 只释放仍属于自己的租锁；释放失败不掩盖业务结果。
    let _ = release_lock(redis_url, profile.id, &owner).await;
    result
}

/// 持锁执行设置（旧版 apply）。Redis 挑战在 SQL 提交后仍可能失败，不能恢复，须重新登记。
#[allow(clippy::too_many_arguments)]
async fn apply_locked(
    pool: &MySqlPool,
    redis_url: &str,
    profile: &Profile,
    sid: &str,
    action: &str,
    password: &str,
    code: &str,
    enrollment_id: &str,
    ip: &str,
) -> Result<Value, MfaError> {
    let mut conn = pool.acquire().await?;
    crate::repo::mfa::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    let account = crate::repo::mfa::lock_active_admin_full_tx(&mut *c, profile.id).await?;
    if account.auth_version != profile.auth_version {
        return Err(MfaError::Credentials);
    }
    let now = chrono::Utc::now();
    let reply = match action {
        "enrol" => {
            if !chargepilot_platform::password::verify(password, &account.password_hash) {
                return Err(MfaError::Proof);
            }
            if account.mfa_enabled {
                return Err(MfaError::State);
            }
            let secret = chargepilot_platform::totp::random_secret();
            let enrollment = random_token(32);
            let uri =
                chargepilot_platform::totp::otpauth_uri("ChargePilot", &account.username, &secret);
            let record = json!({
                "id": enrollment,
                "secret": secret,
                "admin_id": account.id.to_string(),
                "auth_version": account.auth_version.to_string(),
                "session_id": sid,
                "expires_at": (now + chrono::Duration::seconds(ENROLLMENT_TTL_SECS as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            });
            // 登记只写短期缓存，不改变当前 MFA，也不撤销会话。
            save_pending(redis_url, account.id, &record).await?;
            json!({
                "enrollment_id": enrollment,
                "secret": secret,
                "otpauth_uri": uri,
                "expires_in": ENROLLMENT_TTL_SECS,
                "mfa_enabled": false,
                "sessions_revoked": false,
            })
        }
        "confirm" => {
            if account.mfa_enabled {
                return Err(MfaError::State);
            }
            let (pending, raw) = read_pending(redis_url, account.id).await?;
            let pending_admin = pending["admin_id"].as_str().unwrap_or_default().to_string();
            let pending_version = pending["auth_version"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let pending_session = pending["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let expires_at = pending["expires_at"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let pending_id = pending["id"].as_str().unwrap_or_default().to_string();
            let pending_secret = pending["secret"].as_str().unwrap_or_default().to_string();
            let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
                .map(|t| now >= t.with_timezone(&chrono::Utc))
                .unwrap_or(true);
            if pending_admin != account.id.to_string()
                || pending_version != account.auth_version.to_string()
                || pending_session != sid
                || expired
                || pending_id != enrollment_id
            {
                return Err(MfaError::Pending);
            }
            let Some(step) = verify_with_step(&pending_secret, code, now.timestamp()) else {
                return Err(MfaError::Proof);
            };
            // 更新前比较并原子删除读取过的挑战；坏码、过期或已替换挑战不能启用 MFA。
            if !consume_pending(redis_url, account.id, &raw).await? {
                return Err(MfaError::Pending);
            }
            crate::repo::mfa::update_mfa_secret_tx(
                &mut *c,
                account.id,
                Some(&pending_secret),
                account.mfa_secret.as_deref(),
                true,
                true,
            )
            .await?;
            // 首次确认消费新密钥时间步；重新登录须等待验证器切换验证码。
            let updated = crate::repo::mfa::lock_active_admin_full_tx(&mut *c, account.id).await?;
            crate::repo::mfa::consume_totp_step_tx(&mut *c, &updated, step).await?;
            crate::repo::write_auth_audit_tx(
                &mut *c,
                &to_admin_account(&updated),
                "mfa_enable",
                ip,
            )
            .await?;
            json!({"mfa_enabled": true, "sessions_revoked": true})
        }
        "disable" => {
            if !chargepilot_platform::password::verify(password, &account.password_hash) {
                return Err(MfaError::Proof);
            }
            if !account.mfa_enabled || account.mfa_secret.is_none() {
                return Err(MfaError::State);
            }
            let secret = match account.mfa_secret.clone() {
                Some(s) => s,
                None => panic!("mfa_secret 已判定存在"),
            };
            let Some(step) = verify_with_step(&secret, code, now.timestamp()) else {
                return Err(MfaError::Proof);
            };
            crate::repo::mfa::consume_totp_step_tx(&mut *c, &account, step).await?;
            clear_pending(redis_url, account.id).await?;
            crate::repo::mfa::update_mfa_secret_tx(
                &mut *c,
                account.id,
                None,
                account.mfa_secret.as_deref(),
                false,
                true,
            )
            .await?;
            let updated = crate::repo::mfa::lock_active_admin_full_tx(&mut *c, account.id).await?;
            crate::repo::write_auth_audit_tx(
                &mut *c,
                &to_admin_account(&updated),
                "mfa_disable",
                ip,
            )
            .await?;
            json!({"mfa_enabled": false, "sessions_revoked": true})
        }
        _ => return Err(MfaError::State),
    };
    tx.commit().await?;
    Ok(reply)
}

/// MfaAccount → 审计所需的账号身份（旧版 WriteAuthAudit 接收 AdminAccount）。
fn to_admin_account(a: &crate::repo::mfa::MfaAccount) -> crate::AdminAccount {
    crate::AdminAccount {
        id: a.id,
        username: a.username.clone(),
        display_name: None,
        password_hash: String::new(),
        role_id: None,
        mfa_enabled: a.mfa_enabled,
        mfa_secret: a.mfa_secret.clone(),
        mfa_last_step: a.mfa_last_step,
        status: chargepilot_domain::operations::AdminUserStatus::Active,
        failed_login_count: 0,
        locked_until: None,
        auth_version: a.auth_version,
    }
}

// ---------------------------------------------------------------------------
// 管理端（POST /api/v1/admin/admin-users/{id}/mfa）
// ---------------------------------------------------------------------------

/// 管理端 MFA 重置（旧版 AccountService.MFA）：不能操作本人；确认密钥须与
/// 锁定账号后读出的密钥一致。登记响应含当前验证码；审计不保存任何材料。
/// MFA 管理端重置参数即旧版契约字段，不引入参数对象。
#[allow(clippy::too_many_arguments)]
pub async fn managed(
    pool: &MySqlPool,
    actor: &Profile,
    target_id: u64,
    action: &str,
    secret_in: &str,
    code: &str,
    ip: &str,
    request_id: &str,
) -> Result<Value, MfaError> {
    if target_id == actor.id {
        return Err(MfaError::SelfMfa);
    }
    if !matches!(action, "enrol" | "confirm" | "disable") {
        return Err(MfaError::InputMsg(
            "请指定 enrol、confirm 或 disable".into(),
        ));
    }
    if action == "confirm" && (secret_in.is_empty() || code.is_empty()) {
        return Err(MfaError::InputMsg("请提供密钥和验证码".into()));
    }
    let mut conn = pool.acquire().await?;
    crate::repo::mfa::set_read_committed(&mut conn).await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;
    crate::repo::lock_customer_admin_role_tx(&mut *c)
        .await
        .map_err(MfaError::from)?;
    let actor_locked = crate::repo::lock_actor_tx(&mut *c, actor.id)
        .await
        .map_err(|_| MfaError::Credentials)?;
    if !actor_locked
        .permissions
        .iter()
        .any(|p| p == "admin_user.update")
    {
        return Err(MfaError::Credentials);
    }
    let before = crate::repo::lock_admin_tx(&mut *c, target_id)
        .await
        .map_err(MfaError::from)?
        .ok_or(MfaError::NotFound)?;
    let account = crate::repo::mfa::lock_active_admin_full_tx(&mut *c, target_id).await?;
    let now = chrono::Utc::now();
    let (reply, audit_action) = match action {
        "enrol" => {
            let secret = chargepilot_platform::totp::random_secret();
            let uri =
                chargepilot_platform::totp::otpauth_uri("ChargePilot", &account.username, &secret);
            let code_now = chargepilot_platform::totp::generate(&secret, now.timestamp())
                .ok_or_else(|| MfaError::Storage(anyhow::anyhow!("TOTP 生成失败")))?;
            crate::repo::mfa::update_mfa_secret_tx(
                &mut *c,
                target_id,
                Some(&secret),
                account.mfa_secret.as_deref(),
                false,
                account.mfa_enabled,
            )
            .await?;
            (
                json!({"secret": secret, "otpauth_uri": uri, "code": code_now, "enabled": false}),
                "mfa_enrol",
            )
        }
        "confirm" => {
            let Some(db_secret) = account.mfa_secret.as_deref() else {
                return Err(MfaError::InputMsg("验证码校验失败".into()));
            };
            let matched = db_secret == secret_in;
            let step = verify_with_step(secret_in, code, now.timestamp());
            if !matched
                || step.is_none()
                || account
                    .mfa_last_step
                    .is_some_and(|s| step.is_some_and(|v| v <= s))
            {
                return Err(MfaError::InputMsg("验证码校验失败".into()));
            }
            let step = match step {
                Some(s) => s,
                None => panic!("验证步长已判定存在"),
            };
            crate::repo::mfa::consume_totp_step_tx(&mut *c, &account, step)
                .await
                .map_err(|_| MfaError::InputMsg("验证码校验失败".into()))?;
            crate::repo::mfa::update_mfa_secret_tx(
                &mut *c,
                target_id,
                account.mfa_secret.as_deref(),
                account.mfa_secret.as_deref(),
                true,
                !account.mfa_enabled,
            )
            .await?;
            (json!({"id": target_id, "mfa_enabled": true}), "mfa_enable")
        }
        "disable" => {
            crate::repo::mfa::update_mfa_secret_tx(
                &mut *c,
                target_id,
                None,
                account.mfa_secret.as_deref(),
                false,
                account.mfa_enabled || account.mfa_secret.is_some(),
            )
            .await?;
            (
                json!({"id": target_id, "mfa_enabled": false}),
                "mfa_disable",
            )
        }
        _ => unreachable!("动作已校验"),
    };
    crate::repo::append_audit_tx(
        &mut *c,
        &actor_locked,
        "admin_user",
        audit_action,
        "admin_user",
        &target_id.to_string(),
        &json!({
            "id": before.id,
            "username": before.username,
            "mfa_enabled": before.mfa_enabled,
            "status": before.status,
        }),
        &reply,
        ip,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(reply)
}

/// 当前管理员 MFA 状态（GET /api/v1/admin/auth/mfa-settings）。
pub async fn status(pool: &MySqlPool, admin_id: u64) -> Result<Value, MfaError> {
    let enabled = crate::repo::mfa::mfa_enabled(pool, admin_id).await?;
    enabled
        .map(|mfa_enabled| json!({"mfa_enabled": mfa_enabled}))
        .ok_or(MfaError::Credentials)
}
