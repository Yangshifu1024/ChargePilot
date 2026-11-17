//! 计费批次的表访问：charge_billing_job / charge_fee_receipt /
//! charge_order / charge_order_pricing / charge_end_receipt /
//! charge_meter_review / charge_manual_settlement / manual_fee_review /
//! payment_order / refund_record（蓝图第五章 repo 层）。

use chargepilot_domain::money::{BizType, ExecutionPolicy, PaymentOrderStatus, RefundStatus};
use chargepilot_domain::settlement::{
    BillingJobStatus as JobStatus, ManualFeeReviewStatus, MeterReviewStatus,
};
use sqlx::{MySqlConnection, MySqlPool};

/// 领取到期的 pending 计费任务（SKIP LOCKED 批次）。
pub async fn due_billing_jobs(pool: &MySqlPool, limit: u32) -> Result<Vec<u64>, sqlx::Error> {
    let ids: Vec<(u64,)> = sqlx::query_as(
        "SELECT `charge_order_id` FROM `charge_billing_job` \
         WHERE `status` = ? AND `next_attempt_at` <= UTC_TIMESTAMP(3) \
         ORDER BY `charge_order_id` LIMIT ? FOR UPDATE SKIP LOCKED",
    )
    .bind(JobStatus::Pending)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

/// completed 订单基础事实（order_no、用户、支付关联）。
pub async fn completed_order_facts(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<(String, u64, Option<u64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `order_no`, `user_id`, `payment_order_id` FROM `charge_order` \
         WHERE `id` = ? AND `status` = 'completed' AND `deleted_at` IS NULL",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await
}

/// 订单冻结快照 JSON。
pub async fn order_snapshot(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await
}

/// 订单冻结快照 JSON（事务内连接版，计量核实复用）。
pub async fn order_snapshot_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 结束计量 JSON。
pub async fn end_meter_json(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await
}

/// 结束计量 JSON（事务内连接版，计量核实复用）。
pub async fn end_meter_json_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await
}

/// 已批准的计量修正（原始与修正 JSON，取最新一条）。
pub async fn approved_meter_review(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CAST(`original_json` AS CHAR), CAST(`corrected_json` AS CHAR) \
         FROM `charge_meter_review` \
         WHERE `charge_order_id` = ? AND `status` = ? ORDER BY `id` DESC LIMIT 1",
    )
    .bind(charge_order_id as i64)
    .bind(MeterReviewStatus::Approved)
    .fetch_optional(pool)
    .await
}

/// 人工最终金额登记行；request_id 随行返回供幂等。
pub async fn manual_settlement(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<(i64, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `electric_cents`, `service_cents`, CAST(`request_id` AS CHAR) \
         FROM `charge_manual_settlement` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await
}

/// 支付单 FOR UPDATE 锁定；核对逻辑在 service 调用方。
pub async fn lock_pay_order_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<Option<(PaymentOrderStatus, i64, i64, u64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT `status`, `paid_cents`, `refunded_cents`, `id` FROM `payment_order` \
         WHERE `id` = ? AND `biz_type` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .bind(BizType::Charge)
    .fetch_optional(&mut *c)
    .await
}

/// 费用回执幂等读取（已入账则仅完成任务）。
pub async fn find_fee_receipt_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`result_json` AS CHAR) FROM `charge_fee_receipt` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    Ok(row.map(|(json,)| json))
}

/// 任务完成（清空错误）。
pub async fn mark_job_done_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_billing_job` SET `status` = ?, `last_error` = NULL WHERE `charge_order_id` = ?")
        .bind(JobStatus::Done)
        .bind(charge_order_id as i64)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 关闭待核实工单（幂等重入路径同事务推进 pending → resolved）。
pub async fn resolve_manual_review_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `manual_fee_review` SET `status` = ?, `resolved_at` = UTC_TIMESTAMP(3) \
         WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(ManualFeeReviewStatus::Resolved)
    .bind(charge_order_id as i64)
    .bind(ManualFeeReviewStatus::Pending)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 登记费用回执（幂等键 uk 唯一）。
pub async fn insert_fee_receipt_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    calculation_no: &str,
    result_json: &str,
    shortfall: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `charge_fee_receipt` (`charge_order_id`, `calculation_no`, `result_json`, `shortfall_cents`) \
         VALUES (?, ?, CAST(? AS JSON), ?)",
    )
    .bind(charge_order_id as i64)
    .bind(calculation_no)
    .bind(result_json)
    .bind(shortfall)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 写入订单 settled 费用分项与总额。
pub async fn set_order_fee_tx(
    c: &mut MySqlConnection,
    electric: i64,
    service: i64,
    total: i64,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_order` SET `electric_cents` = ?, `service_cents` = ?, `total_cents` = ? \
         WHERE `id` = ?",
    )
    .bind(electric)
    .bind(service)
    .bind(total)
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 在途退款合计（当前读，避免高估可退）。
pub async fn reserved_refund_sum_tx(
    c: &mut MySqlConnection,
    payment_order_id: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(`refund_cents`), 0) AS SIGNED) FROM `refund_record` \
         WHERE `payment_order_id` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .bind(RefundStatus::Pending)
    .bind(RefundStatus::Processing)
    .fetch_one(&mut *c)
    .await
}

/// 余款自动退款登记（预付差额）。
pub async fn insert_surplus_refund_tx(
    c: &mut MySqlConnection,
    refund_no: &str,
    payment_order_id: u64,
    user_id: u64,
    charge_order_id: u64,
    surplus: i64,
    month: chrono::NaiveDate,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
         `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(refund_no)
    .bind(payment_order_id as i64)
    .bind(user_id as i64)
    .bind(BizType::Charge)
    .bind(charge_order_id as i64)
    .bind(surplus)
    .bind("实际计费后的预付差额")
    .bind(RefundStatus::Pending)
    .bind(ExecutionPolicy::Automatic)
    .bind(month)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 订单行内状态（证据链分派）。
pub async fn order_status_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<Option<chargepilot_domain::order::ChargeOrderStatus>, sqlx::Error> {
    sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .fetch_optional(&mut *c)
        .await
}

/// 订单 completed → refunding（条件更新；0 行为静默旧语义）。
pub async fn mark_order_refunding_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE `charge_order` SET `status` = ? WHERE `id` = ? AND `status` = ?")
        .bind(chargepilot_domain::order::ChargeOrderStatus::Refunding)
        .bind(charge_order_id as i64)
        .bind(chargepilot_domain::order::ChargeOrderStatus::Completed)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 订单号（转人工工单挂靠；缺失即异常清理态）。
pub async fn order_no_of(
    pool: &MySqlPool,
    charge_order_id: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT `order_no` FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .fetch_optional(pool)
        .await
}

/// 人工核实工单（uk_order 幂等 upsert，重复入队不新增行）。
pub async fn upsert_manual_review_tx(
    c: &mut MySqlConnection,
    charge_order_id: u64,
    order_no: &str,
    reason: &str,
    energy_wh: u64,
    month: &str,
    source_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO `manual_fee_review` \
         (`charge_order_id`, `order_no`, `reason`, `energy_wh`, `created_month`, `source_json`) \
         VALUES (?, ?, ?, ?, ?, CAST(? AS JSON)) \
         ON DUPLICATE KEY UPDATE `charge_order_id` = `charge_order_id`",
    )
    .bind(charge_order_id as i64)
    .bind(order_no)
    .bind(reason)
    .bind(energy_wh as i64)
    .bind(month)
    .bind(source_json)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 任务状态推进（pending → 目标状态，done 不可回退）。
pub async fn defer_job_status_tx(
    c: &mut MySqlConnection,
    status: JobStatus,
    error: &str,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_billing_job` SET `status` = ?, `last_error` = ? \
         WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(status)
    .bind(error)
    .bind(charge_order_id as i64)
    .bind(JobStatus::Pending)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// 退避：attempts+1，下次尝试按次数指数延迟（1<<n 分钟，上限 60）。
pub async fn backoff_job(
    pool: &MySqlPool,
    error: &str,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_billing_job` SET `attempts` = `attempts` + 1, \
         `next_attempt_at` = DATE_ADD(UTC_TIMESTAMP(3), INTERVAL LEAST(POW(2, `attempts`), 60) MINUTE), \
         `last_error` = ? WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(error)
    .bind(charge_order_id as i64)
    .bind(JobStatus::Pending)
    .execute(pool)
    .await?;
    Ok(())
}

/// 任务状态推进（连接池版）。
pub async fn defer_job_status(
    pool: &MySqlPool,
    status: JobStatus,
    error: &str,
    charge_order_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE `charge_billing_job` SET `status` = ?, `last_error` = ? WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(status)
    .bind(error)
    .bind(charge_order_id as i64)
    .bind(JobStatus::Pending)
    .execute(pool)
    .await?;
    Ok(())
}
