//! 刷卡准入事务的表访问：card_operation / card_charge / device_port / user /
//! charge_order / charge_billing_cutoff（蓝图第五章 repo 层）。

use chargepilot_domain::card::{CardOperationKind, CardOperationStatus};
use sqlx::{MySqlConnection, MySqlPool};

/// 端口编码所属设备号（device_port 与 card_operation 的排序规则不同，比较放应用层）。
pub async fn swipe_device_id(
    pool: &MySqlPool,
    port_code: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT `device_id` FROM `device_port` WHERE `port_code` = ? LIMIT 1")
        .bind(port_code)
        .fetch_optional(pool)
        .await
}

/// 幂等快速恢复行（事务外只读；事务内仍有当前读防护并发竞争）。
pub async fn prior_operation(
    pool: &MySqlPool,
    device_id: &str,
    event_id: &str,
) -> Result<Option<(String, u64, String, i64, u16, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `kind`, `charge_order_id`, `status`, `price_cents`, `minutes`, \
            CAST(`operation_id` AS CHAR) AS `operation_id` \
     FROM `card_operation` WHERE `device_id` = ? AND `event_id` = ?",
    )
    .bind(device_id)
    .bind(event_id)
    .fetch_optional(pool)
    .await
}

/// 事务内当前读恢复首次操作（卡锁不刷新 RR 快照，重放须当前读）。
pub async fn lock_prior_operation_tx(
    c: &mut MySqlConnection,
    device_id: &str,
    event_id: &str,
) -> Result<Option<(String, u64, u64, String, i64, u16, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `kind`, `charge_order_id`, `card_id`, `status`, `price_cents`, `minutes`, \
                CAST(`operation_id` AS CHAR) AS `operation_id` \
         FROM `card_operation` WHERE `device_id` = ? AND `event_id` = ? FOR SHARE",
    )
    .bind(device_id)
    .bind(event_id)
    .fetch_optional(&mut *c)
    .await
}

/// 锁读刷卡绑定用户的状态与 openid（intent 列非空；刷卡用户即绑定用户）。
pub async fn lock_card_user_tx(
    c: &mut MySqlConnection,
    user_id: u64,
) -> Result<Option<(chargepilot_domain::identity::UserStatus, Vec<u8>)>, sqlx::Error> {
    sqlx::query_as("SELECT `status`, `openid` FROM `user` WHERE `id` = ? FOR UPDATE")
        .bind(user_id as i64)
        .fetch_optional(&mut *c)
        .await
}

/// 锁当前端口会话（决定首购或续购）。
pub async fn lock_session_tx(
    c: &mut MySqlConnection,
    port_id: &str,
) -> Result<Option<(u64, u64, u16, u16, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `charge_order_id`, `card_id`, `purchased_minutes`, `max_minutes`, \
                CAST(`package_json` AS CHAR) \
         FROM `card_charge` WHERE `active_port` = ? FOR UPDATE",
    )
    .bind(port_id)
    .fetch_optional(&mut *c)
    .await
}

/// 刷卡订单投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
pub type SwipeOrderRow = (
    String,
    u64,
    Option<chrono::DateTime<chrono::Utc>>,
    String,
    Option<u64>,
);

/// 锁读刷卡订单（续购须本人订单且 charging 中）。
pub async fn lock_order_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<Option<SwipeOrderRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `order_no`, `user_id`, `started_at`, `status`, `payment_order_id` \
                 FROM `charge_order` WHERE `id` = ? FOR UPDATE",
    )
    .bind(order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 计费是否已冻结（冻结后续购无效）。
pub async fn cutoff_count_tx(c: &mut MySqlConnection, order_id: u64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?")
        .bind(order_id as i64)
        .fetch_one(&mut *c)
        .await
}

/// 待确认首购操作数（can_extend 的 pending 判定）。
pub async fn pending_confirming_count_tx(
    c: &mut MySqlConnection,
    order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM `card_operation` \
         WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(order_id as i64)
    .bind(CardOperationStatus::Confirming)
    .fetch_one(&mut *c)
    .await
}

/// 续购累计会话已购金额与时长。
pub async fn add_session_paid_tx(
    c: &mut MySqlConnection,
    price_cents: i64,
    minutes: i64,
    order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `card_charge` SET `paid_cents` = `paid_cents` + ?, \
             `purchased_minutes` = `purchased_minutes` + ? WHERE `charge_order_id` = ?",
    )
    .bind(price_cents)
    .bind(minutes)
    .bind(order_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 续购操作记录（立即 confirmed，带确认时刻）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_extend_operation_tx(
    c: &mut MySqlConnection,
    operation_id: &str,
    device_id: &str,
    event_id: &str,
    order_id: u64,
    card_id: u64,
    price_cents: i64,
    minutes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `card_operation` (`operation_id`, `device_id`, `event_id`, \
             `charge_order_id`, `card_id`, `kind`, `price_cents`, `minutes`, \
             `status`, `confirmed_at`) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3))",
    )
    .bind(operation_id)
    .bind(device_id)
    .bind(event_id)
    .bind(order_id as i64)
    .bind(card_id as i64)
    .bind(CardOperationKind::Extend)
    .bind(price_cents)
    .bind(minutes)
    .bind(CardOperationStatus::Confirmed)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 首购会话：active_port 唯一键串行同端口占用，冲突（23000）原样上抛由调用方判定。
#[allow(clippy::too_many_arguments)]
pub async fn insert_charge_session_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    card_id: u64,
    port_id: &str,
    card_no: &str,
    price_cents: i64,
    minutes: i64,
    max_minutes: u16,
    wallet_after_cents: i64,
    package_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `card_charge` (`charge_order_id`, `card_id`, `port_code`, `active_port`, \
         `paid_cents`, `purchased_minutes`, `max_minutes`, `card_no`, `wallet_after_cents`, \
         `package_json`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS JSON))",
    )
    .bind(charge_order_id as i64)
    .bind(card_id as i64)
    .bind(port_id)
    .bind(port_id)
    .bind(price_cents)
    .bind(minutes)
    .bind(max_minutes)
    .bind(card_no)
    .bind(wallet_after_cents)
    .bind(package_json)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 首购操作记录（confirming：启动回执确认）。
#[allow(clippy::too_many_arguments)]
pub async fn insert_start_operation_tx(
    c: &mut MySqlConnection,
    operation_id: &str,
    device_id: &str,
    event_id: &str,
    charge_order_id: u64,
    card_id: u64,
    price_cents: i64,
    minutes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `card_operation` (`operation_id`, `device_id`, `event_id`, \
         `charge_order_id`, `card_id`, `kind`, `price_cents`, `minutes`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(operation_id)
    .bind(device_id)
    .bind(event_id)
    .bind(charge_order_id as i64)
    .bind(card_id as i64)
    .bind(CardOperationKind::Start)
    .bind(price_cents)
    .bind(minutes)
    .bind(CardOperationStatus::Confirming)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 启动回执成功：首购操作 confirming → confirmed。
pub async fn confirm_start_operation_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `card_operation` SET `status` = ?, `confirmed_at` = UTC_TIMESTAMP(3) \
         WHERE `charge_order_id` = ? AND `kind` = ? AND `status` = ?",
    )
    .bind(CardOperationStatus::Confirmed)
    .bind(charge_order_id as i64)
    .bind(CardOperationKind::Start)
    .bind(CardOperationStatus::Confirming)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 启动回执失败：首购操作 confirming → failed（退款由回执链登记）。
pub async fn fail_start_operation_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `card_operation` SET `status` = ? \
         WHERE `charge_order_id` = ? AND `kind` = ? AND `status` = ?",
    )
    .bind(CardOperationStatus::Failed)
    .bind(charge_order_id as i64)
    .bind(CardOperationKind::Start)
    .bind(CardOperationStatus::Confirming)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 结束或失败时释放刷卡会话端口占用（charging 回执联动调用；非刷卡订单无行）。
pub async fn card_charge_port_release_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `card_charge` SET `active_port` = NULL \
         WHERE `charge_order_id` = ? AND `active_port` IS NOT NULL",
    )
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}
