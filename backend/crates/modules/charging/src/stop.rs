//! 用户停止充电（旧版 charging/stop/authorization.go 对齐）：
//! 授权校验 → 同事务冻结计费截止（no-op upsert，重试不延长）→ 下发停止命令。

use crate::sink::CommandSink;
use sqlx::{Acquire, MySqlPool};

/// 停止域错误（HTTP 层映射旧版 Err 族）。
#[derive(Debug, thiserror::Error)]
pub enum StopError {
    /// 订单不存在、不属于该用户或不可停止。
    #[error("订单不存在或不可停止")]
    NotFound,
    /// 端口身份缺失（无成功启动回执）。
    #[error("端口身份缺失")]
    PortIdentity,
    /// 截止点持久化失败。
    #[error("计费截止保存失败: {0}")]
    CutoffUnavailable(String),
    /// 命令下发失败（可重试）。
    #[error("停止命令下发失败: {0}")]
    SendFailed(String),
    /// 存储错误。
    #[error("停止处理存储错误: {0}")]
    Storage(String),
}

impl From<sqlx::Error> for StopError {
    fn from(e: sqlx::Error) -> Self {
        StopError::Storage(e.to_string())
    }
}

/// 停止服务（旧版 AuthorizationService + 网关停止受理）。
pub struct StopService {
    pub pool: MySqlPool,
    pub sink: std::sync::Arc<dyn CommandSink>,
}

/// 活跃停止授权（旧版 ActiveStopAuthorization）。
#[derive(Debug, Clone)]
pub struct ActiveStop {
    pub charge_order_id: u64,
    pub order_no: String,
    pub user_id: u64,
    pub device_id: String,
    pub port_no: u8,
    pub start_command_id: String,
}

impl StopService {
    /// 用户停止入口（旧版 AuthorizationService.Stop）：
    /// 归属与充电中校验 → 计费截止冻结 → 停止命令下发与登记。
    pub async fn stop(&self, order_no: &str, user_id: u64) -> Result<ActiveStop, StopError> {
        // 授权查询：属于用户、charging、有成功启动回执（旧版 FindActiveStopAuthorization）。
        let row: Option<(u64, String, u64, String, u8, String)> = sqlx::query_as(
            "SELECT c.`id`, c.`order_no`, c.`user_id`, c.`device_id`, c.`port_no`, CAST(s.`command_id` AS CHAR) \
             FROM `charge_order` c \
             JOIN `charge_start_receipt` s ON s.`charge_order_id` = c.`id` AND s.`success` = 1 \
             WHERE c.`order_no` = ? AND c.`user_id` = ? AND c.`status` = ? \
               AND c.`deleted_at` IS NULL LIMIT 1",
        )
        .bind(order_no)
        .bind(user_id as i64)
        .bind(chargepilot_domain::order::ChargeOrderStatus::Charging)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, no, uid, device_id, port_no, start_command_id)) = row else {
            return Err(StopError::NotFound);
        };
        let active = ActiveStop {
            charge_order_id: id,
            order_no: no,
            user_id: uid,
            device_id,
            port_no,
            start_command_id,
        };

        // 同事务冻结计费截止 + 写停止命令（no-op upsert：首次冻结，重试不延长）。
        let stop_command_id = chargepilot_platform::ids::uuid_v4();
        let mut conn = self.pool.acquire().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let c = tx.acquire().await?;
        sqlx::query(
            "INSERT INTO `charge_billing_cutoff` (`charge_order_id`, `cutoff_at`, `reason`) \
             VALUES (?, UTC_TIMESTAMP(3), 'stop_request') \
             ON DUPLICATE KEY UPDATE `charge_order_id` = `charge_order_id`",
        )
        .bind(active.charge_order_id as i64)
        .execute(&mut *c)
        .await?;
        // 已有停止命令则复用（幂等重放）。
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT CAST(`command_id` AS CHAR) FROM `charge_stop_command` WHERE `charge_order_id` = ?",
        )
        .bind(active.charge_order_id as i64)
        .fetch_optional(&mut *c)
        .await?;
        let stop_command_id = match existing {
            Some((id,)) => id,
            None => {
                sqlx::query(
                    "INSERT INTO `charge_stop_command` (`command_id`, `start_command_id`, \
                     `charge_order_id`, `order_no`, `user_id`, `device_id`, `port_no`, `port_id`, `status`) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&stop_command_id)
                .bind(&active.start_command_id)
                .bind(active.charge_order_id as i64)
                .bind(&active.order_no)
                .bind(active.user_id as i64)
                .bind(&active.device_id)
                .bind(active.port_no)
                .bind(active.charge_order_id as i64)
                .bind(chargepilot_domain::charging_command::ChargeStopCommandStatus::Pending)
                .execute(&mut *c)
                .await?;
                sqlx::query("UPDATE `charge_command` SET `status` = ? WHERE `charge_order_id` = ?")
                    .bind(chargepilot_domain::charging_command::ChargeCommandStatus::Stopping)
                    .bind(active.charge_order_id as i64)
                    .execute(&mut *c)
                    .await?;
                stop_command_id
            }
        };
        tx.commit().await?;

        // 下发停止命令（失败可重试：截止点已冻结，重复下发等价）。
        self.sink
            .send_stop(&active.device_id.clone(), active.port_no, &stop_command_id)
            .await
            .map_err(StopError::SendFailed)?;
        sqlx::query(
            "UPDATE `charge_stop_command` SET `status` = ?, `sent_at` = UTC_TIMESTAMP(3) \
             WHERE `command_id` = ?",
        )
        .bind(chargepilot_domain::charging_command::ChargeStopCommandStatus::Sent)
        .bind(&stop_command_id)
        .execute(&self.pool)
        .await?;
        Ok(active)
    }
}
