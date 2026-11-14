//! 计费批次（旧版 BillingService.Run + BillingOrders.Apply 的单体对应物）。
//! 领取 pending 任务 → 读订单/快照/计量 → domain 结算（批 1 差分验证的
//! settle_session）→ 资金同事务：费用回执幂等、订单费用列、余款退款登记、
//! 事件出站；计量/快照无效转人工，存储错误退避重试。

use chargepilot_domain::charging_actual::{ActualMeter, EndMeter};
use chargepilot_domain::charging_settle::settle_session;
use chargepilot_domain::charging_spec::BillingError;
use chargepilot_domain::money::{ExecutionPolicy, PaymentBizType, PaymentStatus, RefundStatus};
use sqlx::{Acquire, MySqlPool};

/// 计费任务状态词表（charge_billing_job.status）。
pub struct JobStatus;
impl JobStatus {
    pub const PENDING: &'static str = "pending";
    pub const MANUAL_REVIEW: &'static str = "manual_review";
    pub const DONE: &'static str = "done";
}

/// 领取到期的 pending 计费任务（SKIP LOCKED 批次，旧版 Due）。
pub async fn due_jobs(pool: &MySqlPool, limit: u32) -> anyhow::Result<Vec<u64>> {
    let ids: Vec<(u64,)> = sqlx::query_as(
        "SELECT `charge_order_id` FROM `charge_billing_job` \
         WHERE `status` = ? AND `next_attempt_at` <= UTC_TIMESTAMP(3) \
         ORDER BY `charge_order_id` LIMIT ? FOR UPDATE SKIP LOCKED",
    )
    .bind(JobStatus::PENDING)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

/// 处理单个计费任务；返回 Ok(true) 表示结算完成，Ok(false) 表示转人工或退避。
pub async fn process_job(pool: &MySqlPool, charge_order_id: u64) -> anyhow::Result<bool> {
    // 读取阶段（无事务）：订单 + 快照 + 结束计量。
    let order: Option<(String, u64, Option<u64>)> = sqlx::query_as(
        "SELECT `order_no`, `user_id`, `payment_order_id` FROM `charge_order` \
         WHERE `id` = ? AND `status` = 'completed' AND `deleted_at` IS NULL",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    let Some((order_no, user_id, payment_order_id)) = order else {
        // 订单不在 completed（异常回滚等）：任务转人工，避免空转。
        defer_to_manual_review(pool, charge_order_id, "订单不在已完成状态", 0, None).await?;
        return Ok(false);
    };
    let Some(payment_order_id) = payment_order_id else {
        defer_to_manual_review(pool, charge_order_id, "订单缺少支付关联", 0, None).await?;
        return Ok(false);
    };
    // 快照 rule（spec 为选中套餐的引擎规格）。
    let snapshot_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`pricing_snapshot` AS CHAR) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    let Some(snapshot_json) = snapshot_json else {
        defer_to_manual_review(pool, charge_order_id, "订单缺少计费快照", 0, None).await?;
        return Ok(false);
    };
    let snapshot: chargepilot_mod_payment::checkout::PricingSnapshot =
        serde_json::from_str(&snapshot_json).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let meter_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`meter_json` AS CHAR) FROM `charge_end_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    let Some(meter_json) = meter_json else {
        defer_to_manual_review(pool, charge_order_id, "订单缺少结束计量", 0, None).await?;
        return Ok(false);
    };
    let mut meter: EndMeter =
        serde_json::from_str(&meter_json).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    // 融合已批准的计量修正（旧版 readBillingSource）：原始计量未变时替换分段；
    // 已变（新回执等）则视为无效，交由下方结算校验转人工。
    let approved: Option<(String, String)> = sqlx::query_as(
        "SELECT CAST(`original_json` AS CHAR), CAST(`corrected_json` AS CHAR) \
         FROM `charge_meter_review` \
         WHERE `charge_order_id` = ? AND `status` = 'approved' ORDER BY `id` DESC LIMIT 1",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    if let Some((original_json, corrected_json)) = approved {
        let original: serde_json::Value =
            serde_json::from_str(&original_json).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let corrected: serde_json::Value =
            serde_json::from_str(&corrected_json).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let current = serde_json::json!({ "meter": meter });
        if current == original
            && let Ok(corrected_meter) =
                serde_json::from_value::<EndMeter>(corrected["meter"].clone())
        {
            meter = corrected_meter;
        }
    }

    // 人工最终金额优先（仪表审核后的兜底出口；旧版 FinalFee 语义）：
    // 登记后金额直接入账，仍受预付封顶与回执幂等保护。
    let manual: Option<(i64, i64, String)> = sqlx::query_as(
        "SELECT `electric_cents`, `service_cents`, CAST(`request_id` AS CHAR) \
         FROM `charge_manual_settlement` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(pool)
    .await?;
    let settlement = if let Some((electric, service, _request)) = manual {
        chargepilot_domain::charging_settle::Settlement {
            electric_cents: electric,
            service_cents: service,
            total_cents: electric + service,
            estimated: true,
            ..Default::default()
        }
    } else {
        // domain 结算（批 1 差分验证的入口）：计量校验失败转人工。
        let rule = snapshot.rule.clone();
        let actual = ActualMeter {
            review_required: false,
            started_at: meter.segments.first().map(|s| s.started_at),
            ended_at: Some(meter.ended_at),
            charged_wh: meter.charged_wh,
            charged_seconds: meter.charged_seconds,
            segments: meter.segments.clone(),
        };
        let paid = Some(snapshot.offer.clone());
        match settle_session(&rule.spec, &actual, paid.as_ref(), None) {
            Ok(s) => s,
            Err(BillingError::MeterReview) | Err(BillingError::InvalidPricing) => {
                // 工单快照对齐旧版 readBillingSource：订单/用户/规则/套餐/计量全量。
                let source = serde_json::json!({
                    "charge_order_id": charge_order_id,
                    "order_no": order_no,
                    "user_id": user_id,
                    "rule": snapshot.rule,
                    "offer": snapshot.offer,
                    "meter": meter,
                });
                defer_to_manual_review(
                    pool,
                    charge_order_id,
                    "实际计量或计费快照需核实",
                    u64::from(meter.charged_wh),
                    Some(source.to_string().as_str()),
                )
                .await?;
                return Ok(false);
            }
            Err(e) => {
                defer_job(
                    pool,
                    charge_order_id,
                    JobStatus::PENDING,
                    &format!("计费暂不可用: {e}"),
                )
                .await?;
                return Ok(false);
            }
        }
    };

    // 资金事务：费用回执幂等 → 订单费用列 → 余款退款登记 → 事件 → 任务 done。
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    let c = tx.acquire().await?;

    // 支付单锁定并核对（旧版 Apply 的核心核对集合）。
    let payment: Option<(String, i64, i64, u64)> = sqlx::query_as(
        "SELECT `status`, `paid_cents`, `refunded_cents`, `id` FROM `payment_order` \
         WHERE `id` = ? AND `biz_type` = ? AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(payment_order_id as i64)
    .bind(PaymentBizType::CHARGE)
    .fetch_optional(&mut *c)
    .await?;
    let Some((pay_status, paid_cents, refunded_cents, pay_id)) = payment else {
        tx.rollback().await?;
        defer_to_manual_review(pool, charge_order_id, "支付单缺失或类型不符", 0, None).await?;
        return Ok(false);
    };
    if !matches!(
        pay_status.as_str(),
        PaymentStatus::PAID | PaymentStatus::PARTIAL_REFUNDED
    ) || paid_cents <= 0
        || refunded_cents < 0
        || refunded_cents > paid_cents
    {
        tx.rollback().await?;
        defer_to_manual_review(pool, charge_order_id, "支付单状态不允许结算", 0, None).await?;
        return Ok(false);
    }

    // 费用回执幂等：已入账则仅完成任务（旧版 previous.RowsAffected > 0 分支）。
    let calculation_no = calculation_number(&order_no, charge_order_id);
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT CAST(`result_json` AS CHAR) FROM `charge_fee_receipt` WHERE `charge_order_id` = ? FOR UPDATE",
    )
    .bind(charge_order_id as i64)
    .fetch_optional(&mut *c)
    .await?;
    if existing.is_some() {
        sqlx::query("UPDATE `charge_billing_job` SET `status` = ?, `last_error` = NULL WHERE `charge_order_id` = ?")
            .bind(JobStatus::DONE)
            .bind(charge_order_id as i64)
            .execute(&mut *c)
            .await?;
        // 回执已存在的重放同样关闭待核实工单（幂等重入路径）。
        sqlx::query(
            "UPDATE `manual_fee_review` SET `status` = 'resolved', `resolved_at` = UTC_TIMESTAMP(3) \
             WHERE `charge_order_id` = ? AND `status` = 'pending'",
        )
        .bind(charge_order_id as i64)
        .execute(&mut *c)
        .await?;
        // 账单签发接线：重放路径同事务幂等补签（uk_order 复用，已有账单零写入）。
        crate::bill::issue_bill_in_tx(c, charge_order_id).await?;
        tx.commit().await?;
        return Ok(true);
    }

    // 金额模式封顶：结算总额不得超过预付可退余额（超出部分即缺口，不追缴）。
    let total = settlement.total_cents.min(paid_cents - refunded_cents);
    let shortfall = settlement.total_cents - total;
    sqlx::query(
        "INSERT INTO `charge_fee_receipt` (`charge_order_id`, `calculation_no`, `result_json`, `shortfall_cents`) \
         VALUES (?, ?, CAST(? AS JSON), ?)",
    )
    .bind(charge_order_id as i64)
    .bind(&calculation_no)
    .bind(
        serde_json::json!({
            "charge_order_id": charge_order_id,
            "order_no": order_no,
            "electric_cents": settlement.electric_cents,
            "service_cents": settlement.service_cents,
            "total_cents": total,
            "shortfall_cents": shortfall,
            "mode": settlement.mode,
            "estimated": settlement.estimated,
        })
        .to_string(),
    )
    .bind(shortfall)
    .execute(&mut *c)
    .await?;
    // 订单费用列（旧版 SetSettledFeeInTx）。
    sqlx::query(
        "UPDATE `charge_order` SET `electric_cents` = ?, `service_cents` = ?, `total_cents` = ? \
         WHERE `id` = ?",
    )
    .bind(settlement.electric_cents)
    .bind(settlement.service_cents)
    .bind(total)
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;

    // 余款：实付 - 实费 - 已退 - 在途（当前读，避免高估可退）。
    let reserved: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(`refund_cents`), 0) AS SIGNED) FROM `refund_record` \
         WHERE `payment_order_id` = ? AND `status` IN (?, ?) AND `deleted_at` IS NULL FOR UPDATE",
    )
    .bind(pay_id as i64)
    .bind(RefundStatus::PENDING)
    .bind(RefundStatus::PROCESSING)
    .fetch_one(&mut *c)
    .await?;
    let surplus = paid_cents - total - refunded_cents - reserved;
    let mut refund_no = String::new();
    if surplus > 0 {
        refund_no = format!("FEE-R{charge_order_id:020}");
        sqlx::query(
            "INSERT INTO `refund_record` (`refund_no`, `payment_order_id`, `user_id`, `biz_type`, \
             `biz_id`, `refund_cents`, `reason`, `status`, `execution_policy`, `created_month`) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&refund_no)
        .bind(pay_id as i64)
        .bind(user_id as i64)
        .bind(PaymentBizType::CHARGE)
        .bind(charge_order_id as i64)
        .bind(surplus)
        .bind("实际计费后的预付差额")
        .bind(RefundStatus::PENDING)
        .bind(ExecutionPolicy::AUTOMATIC)
        .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
        .execute(&mut *c)
        .await?;
        // 订单进入退款中（渠道退款成功后置 refunded）。
        sqlx::query("UPDATE `charge_order` SET `status` = 'refunding' WHERE `id` = ? AND `status` = 'completed'")
            .bind(charge_order_id as i64)
            .execute(&mut *c)
            .await?;
    }
    // 订单事件 + 出站事件（结算完成信号）。
    let event_id = format!(
        "B{}",
        &calculation_no[calculation_no.len().saturating_sub(32)..]
    );
    chargepilot_mod_order::append_event_in_tx(
        &mut *c,
        charge_order_id,
        &event_id,
        "fee_calculated",
        "billing",
        &format!(
            "electric={} service={} total={} shortfall={}",
            settlement.electric_cents, settlement.service_cents, total, shortfall
        ),
        chrono::Utc::now().timestamp_micros(),
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    chargepilot_mod_order::append_domain_event_tx(
        &mut *c,
        &event_id,
        "user",
        "charge_settled",
        &serde_json::json!({
            "event_id": event_id,
            "charge_order_id": charge_order_id,
            "order_no": order_no,
            "user_id": user_id,
            "total_cents": total,
            "refund_cents": surplus.max(0),
            "refund_no": refund_no,
        }),
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    sqlx::query("UPDATE `charge_billing_job` SET `status` = ?, `last_error` = NULL WHERE `charge_order_id` = ?")
        .bind(JobStatus::DONE)
        .bind(charge_order_id as i64)
        .execute(&mut *c)
        .await?;
    // 人工核实后的订单成功入账：关闭待核实工单（旧版 fee_calculation_repo
    // 的 pending→resolved 同事务推进）。
    sqlx::query(
        "UPDATE `manual_fee_review` SET `status` = 'resolved', `resolved_at` = UTC_TIMESTAMP(3) \
         WHERE `charge_order_id` = ? AND `status` = 'pending'",
    )
    .bind(charge_order_id as i64)
    .execute(&mut *c)
    .await?;
    // 账单签发接线：费用入账与账单同一事务落地（旧版生产链路的账单缺口由
    // Missing 补偿循环兜底，此处直接消除缺口；total<=0 时签发内部返回 None 跳过）。
    crate::bill::issue_bill_in_tx(c, charge_order_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// 转人工：同事务写入人工核实工单（旧版 EnqueueManualFeeReview）并推进任务。
/// 工单按 uk_order（charge_order_id）幂等——重复入队不新增行；
/// reason 供人工定价参考，source_json 保存来源计量快照。
pub async fn defer_to_manual_review(
    pool: &MySqlPool,
    charge_order_id: u64,
    reason: &str,
    energy_wh: u64,
    source_json: Option<&str>,
) -> anyhow::Result<()> {
    let order_no: Option<String> =
        sqlx::query_scalar("SELECT `order_no` FROM `charge_order` WHERE `id` = ?")
            .bind(charge_order_id as i64)
            .fetch_optional(pool)
            .await?;
    let Some(order_no) = order_no else {
        // 订单行缺失（异常清理）：无工单可挂，仅推进任务避免空转。
        return defer_job(pool, charge_order_id, JobStatus::MANUAL_REVIEW, reason).await;
    };
    // 工单月份列是 varchar(7) 'YYYY-MM'（旧版 Format("2006-01")），取当前 UTC 年月。
    let month = chrono::Utc::now().format("%Y-%m").to_string();
    // 项目事务惯例：先 SET 隔离级别再 begin（事务中 SET 会报错）。
    let mut conn = pool.acquire().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *conn)
        .await?;
    let mut tx = conn.begin().await?;
    sqlx::query(
        "INSERT INTO `manual_fee_review` \
         (`charge_order_id`, `order_no`, `reason`, `energy_wh`, `created_month`, `source_json`) \
         VALUES (?, ?, ?, ?, ?, CAST(? AS JSON)) \
         ON DUPLICATE KEY UPDATE `charge_order_id` = `charge_order_id`",
    )
    .bind(charge_order_id as i64)
    .bind(&order_no)
    .bind(reason)
    .bind(energy_wh as i64)
    .bind(month.to_string())
    .bind(source_json)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE `charge_billing_job` SET `status` = ?, `last_error` = ? \
         WHERE `charge_order_id` = ? AND `status` = ?",
    )
    .bind(JobStatus::MANUAL_REVIEW)
    .bind(reason)
    .bind(charge_order_id as i64)
    .bind(JobStatus::PENDING)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 任务状态推进（manual_review 或退避重试；旧版 Defer 的代际核对在新版简化为
/// 条件更新 pending → 目标状态，done 不可回退）。
pub async fn defer_job(
    pool: &MySqlPool,
    charge_order_id: u64,
    status: &str,
    error: &str,
) -> anyhow::Result<()> {
    if status == JobStatus::PENDING {
        // 退避：attempts+1，下次尝试按次数指数延迟（1<<n 分钟，上限 60）。
        sqlx::query(
            "UPDATE `charge_billing_job` SET `attempts` = `attempts` + 1, \
             `next_attempt_at` = DATE_ADD(UTC_TIMESTAMP(3), INTERVAL LEAST(POW(2, `attempts`), 60) MINUTE), \
             `last_error` = ? WHERE `charge_order_id` = ? AND `status` = ?",
        )
        .bind(error)
        .bind(charge_order_id as i64)
        .bind(JobStatus::PENDING)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("UPDATE `charge_billing_job` SET `status` = ?, `last_error` = ? WHERE `charge_order_id` = ? AND `status` = ?")
            .bind(status)
            .bind(error)
            .bind(charge_order_id as i64)
            .bind(JobStatus::PENDING)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// 计费编号：B + 订单号去前缀 + 订单 ID（旧版 CalculationNumber 的新版对应；
/// 差分语义以账单唯一性为准，编号格式不影响资金结果）。
fn calculation_number(order_no: &str, charge_order_id: u64) -> String {
    let _ = order_no;
    format!("B{charge_order_id:020}")
}
