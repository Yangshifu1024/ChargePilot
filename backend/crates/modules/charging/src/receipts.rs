//! 设备启动/结束回执应用（旧版 charging/start/result_repo.go 与
//! charging/finish/result_repo.go 对齐；本批覆盖微信支付路径，卡随批 6）。
//! 幂等（红线）：同命令同内容重放静默确认，不同内容报冲突。

use chargepilot_domain::charging_actual::{EndMeter, same_segments};
use sqlx::{Acquire, MySqlPool};

/// 启动回执输入（旧版 StartResult）。
#[derive(Debug, Clone)]
pub struct StartReceiptInput {
    pub command_id: String,
    pub charge_order_id: u64,
    pub order_no: String,
    pub device_id: String,
    pub port_no: u8,
    /// 端口主键（active_port_charge 的复合主键）。
    pub port_id: u64,
    pub success: bool,
    pub result_code: u8,
    pub occurred_at_unix_micros: i64,
}

/// 回执冲突（旧版 ErrStartResultConflict / ErrEndResultConflict）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptError {
    /// 回执与订单、启动回执或计量记录不符。
    Conflict,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiptError::Conflict => f.write_str("回执与订单状态不符"),
            ReceiptError::Storage(m) => write!(f, "回执存储错误: {m}"),
        }
    }
}

impl std::error::Error for ReceiptError {}

impl From<sqlx::Error> for ReceiptError {
    fn from(e: sqlx::Error) -> Self {
        ReceiptError::Storage(e.to_string())
    }
}

impl From<chargepilot_mod_order::OrderError> for ReceiptError {
    fn from(e: chargepilot_mod_order::OrderError) -> Self {
        match e {
            chargepilot_mod_order::OrderError::NumberConflict => ReceiptError::Conflict,
            other => ReceiptError::Storage(other.to_string()),
        }
    }
}

/// 应用启动回执（旧版 StartResultService.Apply，微信支付路径）。
/// 返回 true 表示同内容重放。成功 → 订单 charging + 端口占用确认；
/// 失败 → 订单 refunding + 登记渠道退款记录 + 出站退款事件。
pub async fn apply_start_receipt(
    pool: &MySqlPool,
    input: &StartReceiptInput,
) -> Result<bool, ReceiptError> {
    let occurred_at =
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(input.occurred_at_unix_micros)
            .ok_or(ReceiptError::Conflict)?;
    if input.command_id.is_empty()
        || input.charge_order_id == 0
        || input.port_no == 0
        || input.success != (input.result_code == 0)
        || input.result_code > 3
    {
        return Err(ReceiptError::Conflict);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 锁订单并核对设备与端口（user_id 供成功分支写端口占用）。
    let order_no_row: Option<(u64, String, u64, String, u8)> = sqlx::query_as(
        "SELECT `id`, `order_no`, `user_id`, `device_id`, `port_no` FROM `charge_order` \
         WHERE `id` = ? FOR UPDATE",
    )
    .bind(input.charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((id, order_no, view_user_id, device_id, port_no)) = order_no_row else {
        return Err(ReceiptError::Conflict);
    };
    if id != input.charge_order_id
        || order_no != input.order_no
        || device_id != input.device_id
        || port_no != input.port_no
    {
        return Err(ReceiptError::Conflict);
    }
    // 幂等：同命令已登记。同内容静默确认，不同内容冲突（旧版逐列核对）。
    // 启动回执幂等投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
    type StartReceiptRow = (u64, Option<String>, Option<String>, Option<u8>, bool);
    let existing: Option<StartReceiptRow> = sqlx::query_as(
        "SELECT `charge_order_id`, `order_no`, `device_id`, `port_no`, `success` \
         FROM `charge_start_receipt` WHERE `command_id` = ?",
    )
    .bind(&input.command_id)
    .fetch_optional(&mut *c)
    .await?;
    if let Some((existing_order, existing_no, existing_device, existing_port, existing_success)) =
        existing
    {
        if existing_order != input.charge_order_id
            || existing_no.as_deref() != Some(input.order_no.as_str())
            || existing_device.as_deref() != Some(input.device_id.as_str())
            || existing_port != Some(input.port_no)
            || existing_success != input.success
        {
            return Err(ReceiptError::Conflict);
        }
        tx.commit().await?;
        return Ok(true);
    }
    // 状态必须是 paid（旧版 view.Status != paid 冲突）。
    let status: chargepilot_domain::order::ChargeOrderStatus =
        sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
            .bind(id as i64)
            .fetch_one(&mut *c)
            .await?;
    if status != chargepilot_domain::order::ChargeOrderStatus::Paid {
        return Err(ReceiptError::Conflict);
    }
    // 登记回执（uk_command 幂等）。
    sqlx::query(
        "INSERT INTO `charge_start_receipt` \
         (`charge_order_id`, `command_id`, `success`, `port_id`, `order_no`, `device_id`, \
          `port_no`, `result_code`, `occurred_at`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id as i64)
    .bind(&input.command_id)
    .bind(input.success)
    .bind(input.port_id as i64)
    .bind(&input.order_no)
    .bind(&input.device_id)
    .bind(input.port_no)
    .bind(input.result_code)
    .bind(occurred_at)
    .execute(&mut *c)
    .await?;

    let event_id = format!("S{}", &input.command_id[..32.min(input.command_id.len())]);
    if input.success {
        // paid → charging + 启动时间；命令 acked。
        chargepilot_mod_order::mark_charging_tx(&mut *c, id, &order_no, occurred_at).await?;
        // 端口占用（旧版成功分支写 activePortCharge，结束回执按订单释放；
        // 同端口重复占用由唯一键仲裁，重复启动回执在上方幂等分支已拦下）。
        chargepilot_mod_order::occupy_port_tx(
            &mut *c,
            &input.device_id,
            input.port_no,
            input.charge_order_id,
            view_user_id,
            occurred_at,
        )
        .await?;
        // 刷卡首购操作确认（非刷卡订单无行，天然幂等）。
        chargepilot_mod_card::confirm_start_operation_tx(&mut *c, id).await?;
        sqlx::query("UPDATE `charge_command` SET `status` = 'acked', `result_code` = 0, `ack_at` = ?, `result_reported` = 1 WHERE `charge_order_id` = ?")
            .bind(occurred_at)
            .bind(id as i64)
            .execute(&mut *c)
            .await?;
        chargepilot_mod_order::append_event_in_tx(
            &mut *c,
            id,
            &event_id,
            "charge_started",
            "gateway",
            "device start acked",
            input.occurred_at_unix_micros,
        )
        .await
        .map_err(|e| ReceiptError::Storage(e.to_string()))?;
        tx.commit().await?;
        return Ok(false);
    }
    // 失败：订单 refunding（旧版 MarkStartRejectedInTx 语义）+ 登记自动渠道退款
    // + 出站事件；退款执行成功后由执行器收尾 refunding → refunded。
    let reason = format!("设备启动失败（结果码 {}）", input.result_code);
    chargepilot_mod_order::mark_start_rejected_tx(&mut *c, id, &order_no, &reason).await?;
    chargepilot_mod_card::fail_start_operation_tx(&mut *c, id).await?;
    let payment_order_id: u64 = sqlx::query_scalar(
        "SELECT `payment_order_id` FROM `charge_payment_intent` WHERE `charge_order_id` = ?",
    )
    .bind(id as i64)
    .fetch_one(&mut *c)
    .await?;
    let cleaned = input.command_id.replace('-', "");
    let refund_no = format!("WR{}", &cleaned[..cleaned.len().min(32)]);
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
         SELECT ?, ?, c.`user_id`, 'charge', c.`id`, p.`paid_cents`, ?, ?, ?, c.`created_month` \
         FROM `charge_order` c JOIN `payment_order` p ON p.`id` = ? \
         WHERE c.`id` = ?",
    )
    .bind(&refund_no)
    .bind(payment_order_id as i64)
    .bind(&reason)
    .bind(chargepilot_domain::money::RefundStatus::Pending)
    .bind(chargepilot_domain::money::ExecutionPolicy::Automatic)
    .bind(payment_order_id as i64)
    .bind(id as i64)
    .execute(&mut *c)
    .await?;
    sqlx::query("UPDATE `charge_command` SET `status` = 'rejected', `result_code` = ?, `result_reported` = 1, `error` = ? WHERE `charge_order_id` = ?")
        .bind(input.result_code)
        .bind(&reason)
        .bind(id as i64)
        .execute(&mut *c)
        .await?;
    // 释放端口预占（active_port_charge 与意图释放同事务）；刷卡会话占用一并释放。
    chargepilot_mod_order::release_port_tx(&mut *c, &device_id, port_no, id).await?;
    chargepilot_mod_card::card_charge_port_release_tx(&mut *c, id).await?;
    sqlx::query(
        "UPDATE `charge_payment_intent` SET `status` = ? WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(chargepilot_domain::payment_intent::PaymentIntentStatus::RefundRequired)
    .bind(id as i64)
    .bind(chargepilot_domain::payment_intent::PaymentIntentStatus::Paid)
    .execute(&mut *c)
    .await?;
    chargepilot_mod_order::append_domain_event_tx(
        &mut *c,
        &event_id,
        "gateway",
        "charge_start_failed",
        &serde_json::json!({
            "event_id": event_id,
            "charge_order_id": id,
            "order_no": order_no,
            "payment_order_id": payment_order_id,
        }),
    )
    .await
    .map_err(|e| ReceiptError::Storage(e.to_string()))?;
    tx.commit().await?;
    Ok(false)
}

/// 结束回执输入（旧版 EndResult）。
#[derive(Debug, Clone)]
pub struct EndReceiptInput {
    pub charge_order_id: u64,
    pub order_no: String,
    pub start_command_id: String,
    pub stop_command_id: String,
    pub device_id: String,
    pub port_no: u8,
    pub meter: EndMeter,
}

/// 应用结束回执（旧版 EndResultService.Apply）。
/// 返回 true 表示同内容重放。回执、订单完成、端口释放、计费任务与出站事件同事务。
pub async fn apply_end_receipt(
    pool: &MySqlPool,
    input: &EndReceiptInput,
) -> Result<bool, ReceiptError> {
    if input.order_no.is_empty()
        || input.charge_order_id == 0
        || input.start_command_id.is_empty()
        || input.stop_command_id.is_empty()
        || input.device_id.is_empty()
        || input.port_no == 0
    {
        return Err(ReceiptError::Conflict);
    }
    let meter_json =
        serde_json::to_string(&input.meter).map_err(|e| ReceiptError::Storage(e.to_string()))?;

    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 锁订单（卡会话锁随批 6 接入，微信路径先订单）。
    let view = chargepilot_mod_order::lock_for_lifecycle_tx(
        &mut *c,
        input.charge_order_id,
        &input.order_no,
    )
    .await?
    .ok_or(ReceiptError::Conflict)?;
    if view.device_id != input.device_id || view.port_no != input.port_no {
        return Err(ReceiptError::Conflict);
    }
    // 幂等：已有回执 → 同内容重放确认，不同内容冲突。
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(input.charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    if let Some((previous_json,)) = existing {
        let previous: EndMeter = serde_json::from_str(&previous_json)
            .map_err(|e| ReceiptError::Storage(e.to_string()))?;
        if previous.charged_wh != input.meter.charged_wh
            || previous.charged_seconds != input.meter.charged_seconds
            || previous.stop_reason != input.meter.stop_reason
            || !same_segments(&previous.segments, &input.meter.segments)
            || previous.ended_at != input.meter.ended_at
        {
            return Err(ReceiptError::Conflict);
        }
        tx.commit().await?;
        return Ok(true);
    }
    // 状态必须是 charging 且已启动；结束时刻不得早于启动。
    let Some(started_at) = view.started_at else {
        return Err(ReceiptError::Conflict);
    };
    if view.status != chargepilot_domain::order::ChargeOrderStatus::Charging
        || input.meter.ended_at < started_at
    {
        return Err(ReceiptError::Conflict);
    }
    // 成功启动凭据核对（旧版 start.PortID 匹配）。
    let start: Option<(String, bool, Option<u64>)> = sqlx::query_as(
        "SELECT CAST(`command_id` AS CHAR), `success`, `port_id` FROM `charge_start_receipt` \
         WHERE `charge_order_id` = ?",
    )
    .bind(input.charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    let Some((start_cmd, start_success, start_port)) = start else {
        return Err(ReceiptError::Conflict);
    };
    if !start_success
        || start_cmd != input.start_command_id
        || start_port != Some(u64::try_from(input.port_id()).unwrap_or(u64::MAX))
    {
        return Err(ReceiptError::Conflict);
    }
    // 计量校验：分段、总电量与时长一致性（旧版在计费侧校验，这里先存证据）。
    sqlx::query(
        "INSERT INTO `charge_end_receipt` (`charge_order_id`, `stop_command_id`, `meter_json`) \
         VALUES (?, ?, CAST(? AS JSON))",
    )
    .bind(input.charge_order_id as i64)
    .bind(&input.stop_command_id)
    .bind(&meter_json)
    .execute(&mut *c)
    .await?;
    // 订单完成：charging → completed + 计量列（Wh→kWh 四位小数）。
    // Wh → kWh：四位定点小数（旧版 StringFixed(4) 语义；电量非负，除法安全）。
    let kwh = (rust_decimal::Decimal::from(input.meter.charged_wh)
        / rust_decimal::Decimal::from(1000u32))
    .round_dp(4)
    .to_string();
    chargepilot_mod_order::complete_tx(
        &mut *c,
        input.charge_order_id,
        &input.order_no,
        input.meter.ended_at,
        &kwh,
        input.meter.charged_seconds as i64,
    )
    .await?;
    // 释放端口占用（必须匹配本订单）。
    let released = chargepilot_mod_order::release_port_tx(
        &mut *c,
        &input.device_id,
        input.port_no,
        input.charge_order_id,
    )
    .await?;
    if !released {
        return Err(ReceiptError::Conflict);
    }
    // 刷卡会话释放端口占用（非刷卡订单无行，天然幂等）。
    chargepilot_mod_card::card_charge_port_release_tx(&mut *c, input.charge_order_id).await?;
    // 订单事件 + 计费任务 + 出站事件（批 5 的结算批次消费）。
    chargepilot_mod_order::append_event_in_tx(
        &mut *c,
        input.charge_order_id,
        &input.stop_command_id,
        "charge_ended",
        "gateway",
        "device settlement frame accepted",
        input.meter.ended_at.timestamp_micros(),
    )
    .await
    .map_err(|e| ReceiptError::Storage(e.to_string()))?;
    sqlx::query("INSERT IGNORE INTO `charge_billing_job` (`charge_order_id`, `status`) VALUES (?, 'pending')")
        .bind(input.charge_order_id as i64)
        .execute(&mut *c)
        .await?;
    chargepilot_mod_order::append_domain_event_tx(
        &mut *c,
        &input.stop_command_id,
        "gateway",
        "charge_ended",
        &serde_json::json!({
            "event_id": input.stop_command_id,
            "charge_order_id": input.charge_order_id,
            "order_no": input.order_no,
            "stop_command_id": input.stop_command_id,
        }),
    )
    .await
    .map_err(|e| ReceiptError::Storage(e.to_string()))?;
    tx.commit().await?;
    Ok(false)
}

impl EndReceiptInput {
    /// 端口主键（与 active_port_charge 的复合主键一致：即订单 ID）。
    fn port_id(&self) -> i64 {
        self.charge_order_id as i64
    }
}
