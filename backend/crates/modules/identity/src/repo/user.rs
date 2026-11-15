//! `user` 表访问：登录建户、状态检查、profile 组合。

use chargepilot_mod_wallet::account;
use sqlx::MySqlPool;

/// 用户聚合（`user` 表行投影）。
#[derive(Debug, Clone)]
pub struct User {
    pub id: u64,
    pub openid: String,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    pub phone: Option<String>,
    pub status: chargepilot_domain::identity::UserStatus,
}

impl User {
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            chargepilot_domain::identity::UserStatus::Active
        )
    }
}

/// 登录即恢复或建户：同事务完成「查/建用户 + 更新最近登录 + 零余额钱包」。
/// openid 冲突（并发首登）依赖 `uk_live_openid` 唯一键报错，由调用方按存储失败处理。
/// 返回 (用户, 是否新用户)。
pub async fn login_or_create(
    pool: &MySqlPool,
    openid: &str,
    unionid: Option<&str>,
    allocator: &chargepilot_platform::ids::SegmentAllocator,
) -> anyhow::Result<(User, bool)> {
    if openid.is_empty() || openid.len() > 64 {
        anyhow::bail!("invalid wechat identity");
    }
    // 先查后建（单实例下竞态窗口极小；唯一键兜底）
    if let Some(user) = find_active_by_openid(pool, openid).await? {
        sqlx::query("UPDATE `user` SET `last_login_at` = NOW(3) WHERE `id` = ?")
            .bind(user.id)
            .execute(pool)
            .await?;
        return Ok((user, false));
    }
    let mut tx = pool.begin().await?;
    let user_id = allocator.next_id().await?;
    sqlx::query(
        "INSERT INTO `user` (`id`, `openid`, `unionid`, `last_login_at`) VALUES (?, ?, ?, NOW(3))",
    )
    .bind(user_id)
    .bind(openid)
    .bind(unionid)
    .execute(&mut *tx)
    .await?;
    // 零余额钱包与用户同事务（旧版语义）；账户 ID 与用户 ID 同源取号
    let wallet_id = allocator.next_id().await?;
    account::ensure_account_tx(&mut tx, wallet_id, user_id).await?;
    tx.commit().await?;
    let user = find_active_by_openid(pool, openid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("建户后查询失败"))?;
    Ok((user, true))
}

async fn find_active_by_openid(pool: &MySqlPool, openid: &str) -> anyhow::Result<Option<User>> {
    let row = sqlx::query_as::<_, (u64, Option<String>, Option<String>, Option<String>, Option<String>, chargepilot_domain::identity::UserStatus)>(
        "SELECT `id`, `nickname`, `avatar_url`, `phone`, NULL, `status` FROM `user` WHERE `openid` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(openid)
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(id, nickname, avatar_url, phone, _unused, status)| User {
            id,
            openid: openid.to_string(),
            nickname,
            avatar_url,
            phone,
            status,
        }),
    )
}

/// 用户是否处于 active 状态（刷新与 profile 的实时权限检查）。
pub async fn is_active(pool: &MySqlPool, user_id: u64) -> anyhow::Result<bool> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT `status` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(status.as_deref() == Some("active"))
}

/// profile 组合：用户自身字段 + 钱包只读投影。
#[derive(Debug, sqlx::FromRow)]
pub struct UserProfile {
    pub id: u64,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    pub phone: Option<String>,
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

pub async fn find_profile(pool: &MySqlPool, user_id: u64) -> anyhow::Result<Option<UserProfile>> {
    let row = sqlx::query_as::<_, UserProfile>(
        "SELECT `id`, `nickname`, `avatar_url`, `phone`, `first_seen_at` AS `registered_at` \
         FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 事务内锁定用户行并返回状态（旧版 LockStatusInTx 语义：发放/核销前
/// 校验用户可参与营销，锁序为 用户 → 券模板 → 发放行）。
/// 用户不存在或已软删返回 None；调用方按不可参与处理。
pub async fn lock_status_tx(
    tx: &mut sqlx::MySqlConnection,
    user_id: u64,
) -> anyhow::Result<Option<String>> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT `status` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(user_id as i64)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(status)
}
