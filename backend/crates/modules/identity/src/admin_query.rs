//! 后台充电用户查询：列表、档案与统计装饰（旧版 identity/query.go +
//! charge_user_repo.go 对齐）。只读组合：手机号、钱包余额与订单统计按当页
//! ID 批量回填，不逐用户查询；不冻结账号或改写金额。权限复核与旧版一致，
//! 由调用方传入实时 Profile 后再次判定 user.read。

use chargepilot_mod_ops::Profile;
use serde::Serialize;
use sqlx::{MySqlPool, Row};

/// 后台用户查询错误（旧版 operations 哨兵与记录缺失的组合，HTTP 层翻译状态码）。
#[derive(Debug, thiserror::Error)]
pub enum AdminUserError {
    /// 实时权限不足（旧版 operations.ErrForbidden，403/1003）。
    #[error("当前账号无权执行此操作")]
    Forbidden,
    /// 用户不存在或已软删（旧版 gorm.ErrRecordNotFound，404/1004）。
    #[error("记录不存在或已删除")]
    NotFound,
    /// 存储不可用（503/5003）。
    #[error("存储不可用: {0}")]
    Storage(#[from] anyhow::Error),
}

impl From<sqlx::Error> for AdminUserError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(anyhow::anyhow!(e.to_string()))
    }
}

/// 时间列统一按旧版信封格式输出（微秒 + UTC 偏移，前端 dayjs 可解析）。
const TS: &str = "DATE_FORMAT(%C, '%Y-%m-%dT%H:%i:%s.%f+00:00')";

/// 后台充电用户列表的一行（旧版 ChargeUserRow 逐字段对齐）。
/// id / inviter_id 沿用旧版 `,string` 契约按十进制字符串输出；
/// openid / unionid 为 varbinary 列，读侧一律 CAST AS CHAR。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChargeUserRow {
    pub id: String,
    pub openid: String,
    /// 微信 unionid；仅同一开放平台账号下有值。
    pub union_id: Option<String>,
    /// 微信昵称；未授权昵称时为 null，前端回退到「用户 #id」。
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    /// user.gender：unknown/male/female。
    pub gender: String,
    /// user.status：active 正常 / frozen 冻结。
    pub status: String,
    pub first_seen_at: String,
    /// 从未登录过为 null。
    pub last_login_at: Option<String>,
    /// 邀请人用户 ID；自然注册无邀请人。
    pub inviter_id: Option<String>,
    pub created_at: String,
    // 以下派生列不在主查询联表，由 `decorate` 按当页 ID 批量回填。
    pub phone: String,
    pub phone_bound: bool,
    pub balance_cents: i64,
    pub frozen_cents: i64,
    pub order_count: i64,
    pub total_cents: i64,
    /// 最近一次下单时间；从未下过单为 null。
    pub last_order_at: Option<String>,
}

/// 列表响应（旧版 persist.Page 形状 + 权限清单）。
#[derive(Debug, Clone, Serialize)]
pub struct ChargeUserPage {
    pub items: Vec<ChargeUserRow>,
    pub total: i64,
    pub page: u32,
    pub page_size: u32,
    pub permissions: Vec<String>,
}

/// 用户档案中的最近订单摘要（旧版 ChargeUserOrderBrief）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChargeUserOrderBrief {
    pub order_id: u64,
    pub order_no: String,
    pub device_id: String,
    /// 通过 charge_payment_intent 补充；无支付意图的历史订单为 null。
    pub station_id: Option<u64>,
    /// charge_order.status 内部生命周期。
    pub status: String,
    /// business_status 生成列：pending_start/charging/completed。
    pub business_status: String,
    /// 实际到账/退款后持久化的支付状态。
    pub payment_status: String,
    /// 应收合计；未计费为 null。
    pub total_cents: Option<i64>,
    pub created_at: String,
    pub started_at: Option<String>,
}

/// 单个充电用户的档案（旧版 ChargeUserDetail：列表行的超集，扁平输出）。
#[derive(Debug, Clone, Serialize)]
pub struct ChargeUserDetail {
    #[serde(flatten)]
    pub row: ChargeUserRow,
    /// wallet_account.status：active/frozen（与 user.status 取值串相同但分立）。
    pub wallet_status: String,
    /// 最近订单，按下单时间倒序，最多 `DETAIL_ORDER_LIMIT` 条。
    pub recent_orders: Vec<ChargeUserOrderBrief>,
    /// 累计发放的优惠券张数。
    pub coupon_granted: i64,
    /// 当前未使用的优惠券张数。
    pub coupon_unused: i64,
    /// 该用户提交的报障单数。
    pub fault_reports: i64,
}

/// 档案最近订单条数上限（旧版 ChargeUserDetailOrderLimit）。
pub const DETAIL_ORDER_LIMIT: i64 = 20;

/// 关键词模糊查询的转义模式（旧版 dbutil.LikePattern）。
fn like_pattern(value: &str) -> String {
    format!(
        "%{}%",
        value
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    )
}

/// 中国大陆手机号格式（旧版 phone.Valid：trim 后 1[3-9] 开头的 11 位数字）。
fn phone_valid(value: &str) -> bool {
    let v = value.trim();
    v.len() == 11
        && v.starts_with('1')
        && matches!(v.as_bytes().get(1), Some(b'3'..=b'9'))
        && v.bytes().skip(2).all(|b| b.is_ascii_digit())
}

/// 一次性聚合出的派生统计（旧版 ChargeUserStat）。
struct ChargeUserStat {
    phone: String,
    balance_cents: i64,
    frozen_cents: i64,
    order_count: i64,
    total_cents: i64,
    last_order_at: Option<String>,
}

fn require_read(actor: &Profile) -> Result<(), AdminUserError> {
    if actor.permissions.iter().any(|p| p == "user.read") {
        Ok(())
    } else {
        Err(AdminUserError::Forbidden)
    }
}

/// 分页列出充电用户（旧版 UserQueryService.List）。
/// 关键词匹配昵称、openid、unionid 和手机号；完整手机号精确查询，
/// 号码片段模糊查询。派生列按当页 ID 批量回填。
pub async fn list_users(
    pool: &MySqlPool,
    actor: &Profile,
    page: u32,
    page_size: u32,
    keyword: &str,
    status: &str,
) -> Result<ChargeUserPage, AdminUserError> {
    require_read(actor)?;
    /// 关键词过滤方式（旧版语义：完整手机号精确，其余模糊，空值不过滤）。
    enum KwFilter {
        None,
        Phone(String),
        Like,
    }
    let kw = if keyword.is_empty() {
        KwFilter::None
    } else if phone_valid(keyword) {
        KwFilter::Phone(keyword.trim().to_string())
    } else {
        KwFilter::Like
    };
    // LIKE 模式提前计算，生命周期覆盖两个查询构造器。
    let pattern = like_pattern(keyword);
    let mut where_clause = String::from("`deleted_at` IS NULL");
    if !status.is_empty() {
        where_clause.push_str(" AND `status` = ?");
    }
    if !keyword.is_empty() {
        if phone_valid(keyword) {
            where_clause.push_str(" AND `phone` = ?");
        } else {
            where_clause.push_str(
                " AND (`nickname` LIKE ? ESCAPE '!' OR `openid` LIKE ? ESCAPE '!' \
                 OR `unionid` LIKE ? ESCAPE '!' OR `phone` LIKE ? ESCAPE '!')",
            );
        }
    }
    let count_sql = format!("SELECT COUNT(*) FROM `user` WHERE {where_clause}");
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
    if !status.is_empty() {
        count_q = count_q.bind(status);
    }
    match &kw {
        KwFilter::Phone(phone) => count_q = count_q.bind(phone),
        KwFilter::Like => {
            for _ in 0..4 {
                count_q = count_q.bind(&pattern);
            }
        }
        KwFilter::None => {}
    }
    let total = count_q.fetch_one(pool).await?;

    let list_sql = format!(
        "SELECT CAST(`id` AS CHAR) AS `id`, CAST(`openid` AS CHAR) AS `openid`, \
                CAST(`unionid` AS CHAR) AS `union_id`, `nickname`, `avatar_url`, \
                CAST(`gender` AS CHAR) AS `gender`, CAST(`status` AS CHAR) AS `status`, \
                {ts_first} AS `first_seen_at`, {ts_last} AS `last_login_at`, \
                CAST(`inviter_id` AS CHAR) AS `inviter_id`, {ts_created} AS `created_at`, \
                '' AS `phone`, FALSE AS `phone_bound`, \
                0 AS `balance_cents`, 0 AS `frozen_cents`, 0 AS `order_count`, \
                0 AS `total_cents`, NULL AS `last_order_at` \
         FROM `user` WHERE {where_clause} \
         ORDER BY `last_login_at` DESC, `id` DESC LIMIT ? OFFSET ?",
        ts_first = TS.replace("%C", "`first_seen_at`"),
        ts_last = TS.replace("%C", "`last_login_at`"),
        ts_created = TS.replace("%C", "`created_at`"),
    );
    let mut list_q = sqlx::query_as::<_, ChargeUserRow>(&list_sql);
    if !status.is_empty() {
        list_q = list_q.bind(status);
    }
    match &kw {
        KwFilter::Phone(phone) => list_q = list_q.bind(phone),
        KwFilter::Like => {
            for _ in 0..4 {
                list_q = list_q.bind(&pattern);
            }
        }
        KwFilter::None => {}
    }
    let offset = ((page.max(1) - 1) as i64) * (page_size.max(1) as i64);
    let mut rows = list_q
        .bind(page_size.max(1))
        .bind(offset)
        .fetch_all(pool)
        .await?;
    decorate(pool, &mut rows).await?;
    Ok(ChargeUserPage {
        items: rows,
        total,
        page: page.max(1),
        page_size: page_size.max(1),
        permissions: actor.permissions.clone(),
    })
}

/// 按 rows 中的用户 ID 批量回填手机号、钱包与订单统计（旧版 DecorateChargeUsers）。
async fn decorate(pool: &MySqlPool, rows: &mut [ChargeUserRow]) -> Result<(), AdminUserError> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<u64> = rows.iter().filter_map(|r| r.id.parse().ok()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let stats = charge_user_stats(pool, &ids).await?;
    for row in rows.iter_mut() {
        let Ok(id) = row.id.parse::<u64>() else {
            continue;
        };
        let Some(stat) = stats.get(&id) else {
            continue;
        };
        row.phone = stat.phone.clone();
        row.phone_bound = !stat.phone.is_empty();
        row.balance_cents = stat.balance_cents;
        row.frozen_cents = stat.frozen_cents;
        row.order_count = stat.order_count;
        row.total_cents = stat.total_cents;
        row.last_order_at = stat.last_order_at.clone();
    }
    Ok(())
}

/// 按 ids 批量查询手机号、钱包与订单聚合（旧版 ChargeUserStats）。
/// 缺失查询结果保持零值；不假定每用户严格单一钱包（后写覆盖）。
async fn charge_user_stats(
    pool: &MySqlPool,
    ids: &[u64],
) -> Result<std::collections::HashMap<u64, ChargeUserStat>, AdminUserError> {
    let placeholders = vec!["?"; ids.len()].join(",");
    let mut stats: std::collections::HashMap<u64, ChargeUserStat> =
        std::collections::HashMap::new();

    // 手机号（明文；未绑定为 NULL → 空串）。
    let phone_sql = format!(
        "SELECT `id`, CAST(`phone` AS CHAR) AS `phone` FROM `user` WHERE `id` IN ({placeholders})"
    );
    let mut phone_q = sqlx::query(&phone_sql);
    for id in ids {
        phone_q = phone_q.bind(*id as i64);
    }
    for row in phone_q.fetch_all(pool).await? {
        let id: u64 = row.get("id");
        let phone: Option<String> = row.get("phone");
        stats.entry(id).or_insert_with(zero_stat).phone = phone.unwrap_or_default();
    }

    // 钱包余额（仅未删除账户）。
    let wallet_sql = format!(
        "SELECT `user_id`, `balance_cents`, `frozen_cents` FROM `wallet_account` \
         WHERE `deleted_at` IS NULL AND `user_id` IN ({placeholders})"
    );
    let mut wallet_q = sqlx::query(&wallet_sql);
    for id in ids {
        wallet_q = wallet_q.bind(*id as i64);
    }
    for row in wallet_q.fetch_all(pool).await? {
        let user_id: u64 = row.get("user_id");
        let entry = stats.entry(user_id).or_insert_with(zero_stat);
        entry.balance_cents = row.get("balance_cents");
        entry.frozen_cents = row.get("frozen_cents");
    }

    // 订单聚合：仅未删除订单；NULL 金额按 0 计入合计。
    let order_sql = format!(
        "SELECT `user_id`, COUNT(*) AS `order_count`, \
                CAST(COALESCE(SUM(`total_cents`), 0) AS SIGNED) AS `total_cents`, \
                {ts} AS `last_order_at` \
         FROM `charge_order` WHERE `deleted_at` IS NULL AND `user_id` IN ({placeholders}) \
         GROUP BY `user_id`",
        ts = TS.replace("%C", "MAX(`created_at`)")
    );
    let mut order_q = sqlx::query(&order_sql);
    for id in ids {
        order_q = order_q.bind(*id as i64);
    }
    for row in order_q.fetch_all(pool).await? {
        let user_id: u64 = row.get("user_id");
        let entry = stats.entry(user_id).or_insert_with(zero_stat);
        entry.order_count = row.get("order_count");
        entry.total_cents = row.get("total_cents");
        entry.last_order_at = row.get("last_order_at");
    }
    Ok(stats)
}

fn zero_stat() -> ChargeUserStat {
    ChargeUserStat {
        phone: String::new(),
        balance_cents: 0,
        frozen_cents: 0,
        order_count: 0,
        total_cents: 0,
        last_order_at: None,
    }
}

/// 读取未删除用户的档案本体（旧版 LoadChargeUserDetail）：
/// 用户行、钱包状态、最近订单与券/报障计数；用户缺失返回 NotFound。
pub async fn load_user_detail(
    pool: &MySqlPool,
    id: u64,
    order_limit: i64,
) -> Result<ChargeUserDetail, AdminUserError> {
    let row = sqlx::query_as::<_, ChargeUserRow>(&format!(
        "SELECT CAST(`id` AS CHAR) AS `id`, CAST(`openid` AS CHAR) AS `openid`, \
                CAST(`unionid` AS CHAR) AS `union_id`, `nickname`, `avatar_url`, \
                CAST(`gender` AS CHAR) AS `gender`, CAST(`status` AS CHAR) AS `status`, \
                {ts_first} AS `first_seen_at`, {ts_last} AS `last_login_at`, \
                CAST(`inviter_id` AS CHAR) AS `inviter_id`, {ts_created} AS `created_at`, \
                '' AS `phone`, FALSE AS `phone_bound`, \
                0 AS `balance_cents`, 0 AS `frozen_cents`, 0 AS `order_count`, \
                0 AS `total_cents`, NULL AS `last_order_at` \
         FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL",
        ts_first = TS.replace("%C", "`first_seen_at`"),
        ts_last = TS.replace("%C", "`last_login_at`"),
        ts_created = TS.replace("%C", "`created_at`"),
    ))
    .bind(id as i64)
    .fetch_optional(pool)
    .await?
    .ok_or(AdminUserError::NotFound)?;

    // 钱包状态与 user.status 取值串相同但分立；无钱包行为空串（旧版 Scan 零值）。
    let wallet_status: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`status` AS CHAR) FROM `wallet_account` \
         WHERE `user_id` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(id as i64)
    .fetch_optional(pool)
    .await?;

    // 最近订单经 charge_payment_intent 补充站点 ID；其订单唯一键保证不增加行数。
    let recent_orders = sqlx::query_as::<_, ChargeUserOrderBrief>(&format!(
        "SELECT c.`id` AS `order_id`, CAST(c.`order_no` AS CHAR) AS `order_no`, \
                CAST(c.`device_id` AS CHAR) AS `device_id`, i.`station_id`, \
                CAST(c.`status` AS CHAR) AS `status`, \
                CAST(c.`business_status` AS CHAR) AS `business_status`, \
                CAST(c.`payment_status` AS CHAR) AS `payment_status`, \
                c.`total_cents`, {ts_created} AS `created_at`, {ts_started} AS `started_at` \
         FROM `charge_order` c \
         LEFT JOIN `charge_payment_intent` i ON i.`charge_order_id` = c.`id` \
         WHERE c.`user_id` = ? AND c.`deleted_at` IS NULL \
         ORDER BY c.`id` DESC LIMIT ?",
        ts_created = TS.replace("%C", "c.`created_at`"),
        ts_started = TS.replace("%C", "c.`started_at`")
    ))
    .bind(id as i64)
    .bind(order_limit)
    .fetch_all(pool)
    .await?;

    // 仅聚合券与报障数量，不加载明细；unused 取 coupon_grant.status 未核销值。
    let (coupon_granted, coupon_unused, fault_reports): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM `coupon_grant` WHERE `user_id` = ? AND `deleted_at` IS NULL) AS `coupon_granted`, \
           (SELECT COUNT(*) FROM `coupon_grant` WHERE `user_id` = ? AND `deleted_at` IS NULL AND `status` = 'unused') AS `coupon_unused`, \
           (SELECT COUNT(*) FROM `device_fault_report` WHERE `user_id` = ? AND `deleted_at` IS NULL) AS `fault_reports`",
    )
    .bind(id as i64)
    .bind(id as i64)
    .bind(id as i64)
    .fetch_one(pool)
    .await?;

    Ok(ChargeUserDetail {
        row,
        wallet_status: wallet_status.unwrap_or_default(),
        recent_orders,
        coupon_granted,
        coupon_unused,
        fault_reports,
    })
}

/// 单个用户档案（旧版 UserQueryService.Detail）：读取后按单行批量回填统计。
pub async fn user_detail(
    pool: &MySqlPool,
    actor: &Profile,
    id: u64,
) -> Result<ChargeUserDetail, AdminUserError> {
    require_read(actor)?;
    let mut detail = load_user_detail(pool, id, DETAIL_ORDER_LIMIT).await?;
    let mut rows = vec![detail.row.clone()];
    decorate(pool, &mut rows).await?;
    detail.row = rows.remove(0);
    Ok(detail)
}

/// 核对用户未删除（旧版 RequireUserExists）；冻结用户仍可读取历史。
pub async fn require_user_exists(pool: &MySqlPool, id: u64) -> Result<(), AdminUserError> {
    // id 列为 BIGINT UNSIGNED，读侧按 u64 解码。
    let found: Option<u64> =
        sqlx::query_scalar("SELECT `id` FROM `user` WHERE `id` = ? AND `deleted_at` IS NULL")
            .bind(id as i64)
            .fetch_optional(pool)
            .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(AdminUserError::NotFound),
    }
}
