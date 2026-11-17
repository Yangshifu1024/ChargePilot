//! 用户停止充电：
//! 授权校验 → 同事务冻结计费截止（no-op upsert，重试不延长）→ 下发停止命令。
//! SQL 在 repo/stop.rs。

use crate::error::StopError;
use crate::repo::{set_read_committed, stop};
use crate::sink::CommandSink;
use sqlx::{Acquire, MySqlPool};

/// 停止服务。
pub struct StopService {
    pub pool: MySqlPool,
    pub sink: std::sync::Arc<dyn CommandSink>,
}

/// 活跃停止授权。
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
    /// 用户停止入口：
    /// 归属与充电中校验 → 计费截止冻结 → 停止命令下发与登记。
    pub async fn stop(&self, order_no: &str, user_id: u64) -> Result<ActiveStop, StopError> {
        // 授权查询：属于用户、charging、有成功启动回执。
        let Some((id, no, uid, device_id, port_no, start_command_id)) =
            stop::find_active_stop(&self.pool, order_no, user_id).await?
        else {
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
        set_read_committed(&mut conn).await?;
        let mut tx = conn.begin().await?;
        let c = tx.acquire().await?;
        stop::upsert_cutoff_tx(&mut *c, active.charge_order_id).await?;
        let stop_command_id =
            match stop::find_stop_command_tx(&mut *c, active.charge_order_id).await? {
                Some(id) => id,
                None => {
                    stop::insert_stop_command_tx(
                        &mut *c,
                        &stop_command_id,
                        &active.start_command_id,
                        active.charge_order_id,
                        &active.order_no,
                        active.user_id,
                        &active.device_id,
                        active.port_no,
                    )
                    .await?;
                    stop::mark_command_stopping_tx(&mut *c, active.charge_order_id).await?;
                    stop_command_id
                }
            };
        tx.commit().await?;

        // 下发停止命令（失败可重试：截止点已冻结，重复下发等价）。
        self.sink
            .send_stop(&active.device_id.clone(), active.port_no, &stop_command_id)
            .await
            .map_err(StopError::SendFailed)?;
        stop::mark_stop_command_sent_tx(&self.pool, &stop_command_id).await?;
        Ok(active)
    }
}
