//! 刷卡准入与扣款事务（旧版 card/swipe.go + card_repo.go applyInTx 的单体对应物）。
//! 准入：输入校验 → 事件幂等恢复 → 30 秒时效 → 端口解析 → 设备条件 → 方案读取；
//! 事务：卡 → 钱包 → 会话 → 订单锁序内完成扣款、订单/意图/快照/会话写入、
//! 操作记录与出站事件；首购出站 charge_payment_confirmed 驱动启动投递。

use crate::repo::{active_session_count, find_card_charge};
use chargepilot_domain::card::{
    CardOperationKind, CardOperationStatus, CardStatus, can_extend, valid_card_no,
};
use chargepilot_domain::charging_actual::Rule;
use chargepilot_domain::charging_settle::{Estimate, Offer};
use chargepilot_domain::charging_spec::ChargeMode;
use chargepilot_mod_order::{
    PaidOrderDraft, append_domain_event_tx, append_event_in_tx, create_paid_in_tx,
    freeze_pricing_in_tx, new_charge_order_no_tx, new_payment_order_no,
};
use chargepilot_mod_payment::card_funding::{
    checkout_port_available, create_card_intent_in_tx, create_card_payment_in_tx,
    extend_card_payment_in_tx, set_card_order_in_tx,
};
use chargepilot_mod_payment::checkout::PricingSnapshot;
use chargepilot_mod_payment::scan::PortScanner;
use chargepilot_mod_pricing::metering::active_device_rule;
use chargepilot_mod_wallet::ledger::{lock_tx, move_tx};
use chargepilot_platform::ids::{SegmentAllocator, uuid_v4};
use sqlx::{Acquire, MySqlPool};

/// 刷卡准入输入（旧版 SwipeInput）；occurred_at_unix_micros 为 0 视为缺失。
#[derive(Debug, Clone)]
pub struct SwipeInput {
    /// 设备十进制卡号（规范文本，32 位协议范围）。
    pub card_no: String,
    /// 网关冻结的事件号，同事件不重复扣款。
    pub event_id: String,
    /// 网关真实端口编码。
    pub port_id: String,
    /// 首次事件时刻（UTC 微秒）；重放不重新检查时效。
    pub occurred_at_unix_micros: i64,
}

/// 刷卡处理结果（旧版 CardOperation 的对外形态）。
#[derive(Debug, Clone)]
pub struct CardOperationResult {
    pub operation_id: String,
    pub charge_order_id: u64,
    /// start 首购 / extend 续购。
    pub kind: String,
    /// 本次扣款金额，分。
    pub price_cents: i64,
    /// 本次购买时长，分钟。
    pub minutes: u16,
    /// confirming（首购待启动确认）/ confirmed（续购或已确认）。
    pub status: String,
    /// 同事件重放为真。
    pub replayed: bool,
}

/// 刷卡域错误（旧版 SwipeError 阶段 + 哨兵语义）。HTTP/网关把
/// CardDeclined / CardBalance / Conflict 映射为 409 + 业务码 2009。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwipeError {
    /// 输入不合法（卡号、事件号或端口编码）。
    Input,
    /// 事件超时或来自未来。
    Expired,
    /// 端口解析失败或离线。
    Port,
    /// 设备停用。
    Paused,
    /// 协议不支持在线卡。
    Unsupported,
    /// 方案缺失或无效，或卡套餐未配置。
    Pricing,
    /// 端口已有未完成支付或占用。
    Conflict,
    /// 卡状态、余额、端口、累计上限或订单状态不允许刷卡（2009 族）。
    CardDeclined,
    /// 钱包可用余额不足（2009 族）。
    CardBalance,
    /// 存储错误。
    Persist(String),
}

impl std::fmt::Display for SwipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwipeError::Input => f.write_str("刷卡请求参数无效"),
            SwipeError::Expired => f.write_str("刷卡事件已超时"),
            SwipeError::Port => f.write_str("充电端口不可用"),
            SwipeError::Paused => f.write_str("设备已停用"),
            SwipeError::Unsupported => f.write_str("设备不支持在线卡"),
            SwipeError::Pricing => f.write_str("充电方案不可用"),
            SwipeError::Conflict => f.write_str("端口存在进行中的支付或充电"),
            SwipeError::CardDeclined => {
                f.write_str("刷卡不可用：卡状态、余额、端口、累计上限或设备确认状态不满足要求")
            }
            SwipeError::CardBalance => f.write_str("刷卡不可用：钱包可用余额不足"),
            SwipeError::Persist(m) => write!(f, "刷卡处理存储错误: {m}"),
        }
    }
}

impl std::error::Error for SwipeError {}

impl From<sqlx::Error> for SwipeError {
    fn from(e: sqlx::Error) -> Self {
        SwipeError::Persist(e.to_string())
    }
}

impl SwipeError {
    /// 是否 409/2009 拒绝族（明确拒绝，可安全应答设备；网络不确定不算）。
    pub fn is_declined(&self) -> bool {
        matches!(
            self,
            SwipeError::CardDeclined | SwipeError::CardBalance | SwipeError::Conflict
        )
    }
}

/// 刷卡服务：端口解析经 PortScanner 窄接口注入（阶段 06 前为直查桩）。
pub struct SwipeService {
    pub pool: MySqlPool,
    pub allocator: SegmentAllocator,
    pub scanner: std::sync::Arc<dyn PortScanner>,
}

/// 刷卡端口编码白名单（旧版 swipePortPattern）。
fn valid_port_id(port_id: &str) -> bool {
    !port_id.is_empty()
        && port_id.len() <= 64
        && port_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))
}

impl SwipeService {
    /// 刷卡入口（旧版 SwipeService.Apply）：先按事件恢复首次操作，再检查时效
    /// 与设备条件；扣款、订单与会话写入在单一事务内提交。
    pub async fn apply(&self, input: &SwipeInput) -> Result<CardOperationResult, SwipeError> {
        if !valid_port_id(&input.port_id)
            || !valid_card_no(&input.card_no)
            || input.event_id.trim().is_empty()
            || input.event_id.len() > 128
        {
            return Err(SwipeError::Input);
        }
        // 幂等快速恢复（事务外只读；事务内仍有当前读防护并发竞争）。
        // device_port 与 card_operation 的 device_id 排序规则不同，跨表比较放应用层。
        let swipe_device_id: Option<String> = sqlx::query_scalar(
            "SELECT `device_id` FROM `device_port` WHERE `port_code` = ? LIMIT 1",
        )
        .bind(&input.port_id)
        .fetch_optional(&self.pool)
        .await?;
        let prior: Option<(String, u64, String, i64, u16, String)> = match swipe_device_id {
            Some(d) => {
                sqlx::query_as(
                    "SELECT `kind`, `charge_order_id`, `status`, `price_cents`, `minutes`, \
                        CAST(`operation_id` AS CHAR) AS `operation_id` \
                 FROM `card_operation` WHERE `device_id` = ? AND `event_id` = ?",
                )
                .bind(d)
                .bind(&input.event_id)
                .fetch_optional(&self.pool)
                .await?
            }
            None => None,
        };
        if let Some((kind, charge_order_id, status, price_cents, minutes, operation_id)) = prior {
            // 同事件已受理：核对会话归属后原样返回（旧版 replayed 分支）。
            let session = find_card_charge(&self.pool, charge_order_id)
                .await?
                .filter(|s| s.port_code == input.port_id);
            if session.is_some() {
                return Ok(CardOperationResult {
                    operation_id,
                    charge_order_id,
                    kind,
                    price_cents,
                    minutes,
                    status,
                    replayed: true,
                });
            }
            return Err(SwipeError::CardDeclined);
        }

        // 时效：缺失、超过 30 秒或来自未来 1 秒以外拒绝（重放不重新检查）。
        let now = chrono::Utc::now();
        let occurred = if input.occurred_at_unix_micros == 0 {
            None
        } else {
            chrono::DateTime::from_timestamp_micros(input.occurred_at_unix_micros)
        };
        let Some(occurred) = occurred else {
            return Err(SwipeError::Expired);
        };
        if now - occurred > chrono::Duration::seconds(30)
            || occurred > now + chrono::Duration::seconds(1)
        {
            return Err(SwipeError::Expired);
        }

        // 端口解析与设备条件（阶段 06 前直查表，设备视为在线）。
        let resolved = self
            .scanner
            .resolve_scan(&input.port_id)
            .await
            .map_err(|e| match e {
                chargepilot_mod_payment::scan::ScanError::NotFound => SwipeError::Port,
                chargepilot_mod_payment::scan::ScanError::Gateway(m) => SwipeError::Persist(m),
            })?;
        if resolved.kind != "port" {
            return Err(SwipeError::Port);
        }
        let Some(scan_port) = resolved.port.clone() else {
            return Err(SwipeError::Port);
        };
        if resolved.device_status != "enabled" {
            return Err(SwipeError::Paused);
        }
        // 协议能力：阶段 06 前唯一适配器 dc589 支持在线卡；接入新协议时在此
        // 按适配器能力拒绝（Unsupported 保留）。
        let active_sessions = active_session_count(&self.pool, &input.port_id).await?;
        // 首购才需要方案；续购沿用会话冻结套餐。
        let rule = if active_sessions == 0 {
            let active =
                active_device_rule(&self.pool, resolved.station_id, Some(&resolved.device_id))
                    .await
                    .map_err(|_| SwipeError::Pricing)?;
            if active.scheme.card.package_id == 0 {
                return Err(SwipeError::Pricing);
            }
            Some(active)
        } else {
            None
        };

        self.apply_in_tx(input, resolved.station_id, scan_port, rule)
            .await
    }

    /// 扣款事务（旧版 applyInTx）：锁序 卡 → 钱包 → 会话 → 订单；
    /// 首购同事务写支付单、订单、意图、快照、会话与出站事件。
    #[allow(clippy::too_many_lines)]
    async fn apply_in_tx(
        &self,
        input: &SwipeInput,
        station_id: u64,
        port: chargepilot_mod_payment::scan::ScanPort,
        rule: Option<chargepilot_mod_pricing::metering::ActiveRule>,
    ) -> Result<CardOperationResult, SwipeError> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let c = tx.acquire().await?;

        // 锁卡（串行同卡操作）。
        let card = crate::repo::find_online_card(&mut *c, &input.card_no, true)
            .await?
            .ok_or(SwipeError::CardDeclined)?;
        // 事务内当前读恢复首次操作（卡锁不刷新 RR 快照，重放须当前读）。
        let previous: Option<(String, u64, u64, String, i64, u16, String)> = sqlx::query_as(
            "SELECT `kind`, `charge_order_id`, `card_id`, `status`, `price_cents`, `minutes`, \
                    CAST(`operation_id` AS CHAR) AS `operation_id` \
             FROM `card_operation` WHERE `device_id` = ? AND `event_id` = ? FOR SHARE",
        )
        .bind(resolved_device_id(&port))
        .bind(&input.event_id)
        .fetch_optional(&mut *c)
        .await?;
        if let Some((
            kind,
            charge_order_id,
            prev_card_id,
            status,
            price_cents,
            minutes,
            operation_id,
        )) = previous
        {
            if prev_card_id != card.id {
                return Err(SwipeError::CardDeclined);
            }
            let session = find_card_charge(&self.pool, charge_order_id)
                .await?
                .filter(|s| s.port_code == input.port_id);
            if session.is_none() {
                return Err(SwipeError::CardDeclined);
            }
            tx.commit().await?;
            return Ok(CardOperationResult {
                operation_id,
                charge_order_id,
                kind,
                price_cents,
                minutes,
                status,
                replayed: true,
            });
        }
        if card.status != CardStatus::Active {
            return Err(SwipeError::CardDeclined);
        }
        // 用户可用性与 openid（intent 列非空；刷卡用户即绑定用户）。
        let user: Option<(String, Vec<u8>)> =
            sqlx::query_as("SELECT `status`, `openid` FROM `user` WHERE `id` = ? FOR UPDATE")
                .bind(card.user_id as i64)
                .fetch_optional(&mut *c)
                .await?;
        let Some((user_status, openid)) = user else {
            return Err(SwipeError::CardDeclined);
        };
        if user_status != "active" {
            return Err(SwipeError::CardDeclined);
        }
        let mut wallet = lock_tx(&mut *c, card.user_id)
            .await
            .map_err(map_wallet_error)?;

        // 锁当前端口会话（决定首购或续购）。
        let session: Option<(u64, u64, u16, u16, String)> = sqlx::query_as(
            "SELECT `charge_order_id`, `card_id`, `purchased_minutes`, `max_minutes`, \
                    CAST(`package_json` AS CHAR) \
             FROM `card_charge` WHERE `active_port` = ? FOR UPDATE",
        )
        .bind(&port.port_id)
        .fetch_optional(&mut *c)
        .await?;

        let operation_id = uuid_v4();
        let now = chrono::Utc::now();
        let now_micros = now.timestamp_micros();
        let (kind, order_id, package, status): (
            String,
            u64,
            chargepilot_domain::charging_scheme::Package,
            String,
        ) = match session {
            Some((order_id, session_card_id, purchased, max_minutes, package_json)) => {
                // 续购：沿用冻结套餐，不读当前价格。
                let package: chargepilot_domain::charging_scheme::Package =
                    serde_json::from_str(&package_json).map_err(|_| SwipeError::CardDeclined)?;
                // 刷卡订单投影元组（列序即 SELECT 顺序）——type_complexity 要求具名。
                type SwipeOrderRow = (
                    String,
                    u64,
                    Option<chrono::DateTime<chrono::Utc>>,
                    String,
                    Option<u64>,
                );
                let order: Option<SwipeOrderRow> = sqlx::query_as(
                    "SELECT `order_no`, `user_id`, `started_at`, `status`, `payment_order_id` \
                             FROM `charge_order` WHERE `id` = ? FOR UPDATE",
                )
                .bind(order_id as i64)
                .fetch_optional(&mut *c)
                .await?;
                let Some((order_no, order_user, started_at, order_status, payment_order_id)) =
                    order
                else {
                    return Err(SwipeError::CardDeclined);
                };
                let Some(started_at) = started_at else {
                    return Err(SwipeError::CardDeclined);
                };
                if order_user != card.user_id || order_status != "charging" {
                    return Err(SwipeError::CardDeclined);
                }
                // 剩余时长耗尽或计费已冻结：续购无效。
                if now >= started_at + chrono::Duration::minutes(purchased as i64) {
                    return Err(SwipeError::CardDeclined);
                }
                let frozen: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
                )
                .bind(order_id as i64)
                .fetch_one(&mut *c)
                .await?;
                if frozen != 0 {
                    return Err(SwipeError::CardDeclined);
                }
                let pending: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM `card_operation` \
                         WHERE `charge_order_id` = ? AND `status` = ?",
                )
                .bind(order_id as i64)
                .bind(CardOperationStatus::Confirming)
                .fetch_one(&mut *c)
                .await?;
                let available = wallet.balance_cents - wallet.frozen_cents;
                if !can_extend(
                    card.id,
                    session_card_id,
                    &package,
                    purchased,
                    max_minutes,
                    available,
                    pending != 0,
                ) {
                    if can_extend(
                        card.id,
                        session_card_id,
                        &package,
                        purchased,
                        max_minutes,
                        package.price_cents,
                        pending != 0,
                    ) {
                        return Err(SwipeError::CardBalance);
                    }
                    return Err(SwipeError::CardDeclined);
                }
                let Some(payment_order_id) = payment_order_id else {
                    return Err(SwipeError::CardDeclined);
                };
                extend_card_payment_in_tx(&mut *c, payment_order_id, package.price_cents)
                    .await
                    .map_err(|_| SwipeError::CardDeclined)?;
                sqlx::query(
                    "UPDATE `card_charge` SET `paid_cents` = `paid_cents` + ?, \
                         `purchased_minutes` = `purchased_minutes` + ? WHERE `charge_order_id` = ?",
                )
                .bind(package.price_cents)
                .bind(package.minutes as i64)
                .bind(order_id as i64)
                .execute(&mut *c)
                .await?;
                move_tx(
                    &mut *c,
                    card.user_id,
                    &mut wallet,
                    -package.price_cents,
                    &format!("CP{operation_id}"),
                    "在线刷卡extend",
                )
                .await
                .map_err(map_wallet_error)?;
                sqlx::query(
                    "INSERT INTO `card_operation` (`operation_id`, `device_id`, `event_id`, \
                         `charge_order_id`, `card_id`, `kind`, `price_cents`, `minutes`, \
                         `status`, `confirmed_at`) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3))",
                )
                .bind(&operation_id)
                .bind(resolved_device_id(&port))
                .bind(&input.event_id)
                .bind(order_id as i64)
                .bind(card.id as i64)
                .bind(CardOperationKind::Extend)
                .bind(package.price_cents)
                .bind(package.minutes as i64)
                .bind(CardOperationStatus::Confirmed)
                .execute(&mut *c)
                .await?;
                append_event_in_tx(
                    &mut *c,
                    order_id,
                    &operation_id,
                    "card_extend_confirmed",
                    "card",
                    &format!(
                        "card={} cents={} minutes={}",
                        card.id, package.price_cents, package.minutes
                    ),
                    now_micros,
                )
                .await
                .map_err(map_order_error)?;
                let _ = order_no;
                tx.commit().await?;
                return Ok(CardOperationResult {
                    operation_id,
                    charge_order_id: order_id,
                    kind: CardOperationKind::Extend.as_str().to_owned(),
                    price_cents: package.price_cents,
                    minutes: package.minutes.min(u16::MAX as u64) as u16,
                    status: CardOperationStatus::Confirmed.as_str().to_owned(),
                    replayed: false,
                });
            }
            None => {
                // 首购：端口无支付占用、套餐可购才创建订单。
                if !checkout_port_available(&mut *c, &port.port_id).await? {
                    return Err(SwipeError::Conflict);
                }
                if !port.available {
                    return Err(SwipeError::CardDeclined);
                }
                let Some(active) = rule.as_ref() else {
                    return Err(SwipeError::Pricing);
                };
                let Some(package) = active.scheme.package(active.scheme.card.package_id) else {
                    return Err(SwipeError::Pricing);
                };
                let available = wallet.balance_cents - wallet.frozen_cents;
                if !can_extend(
                    card.id,
                    card.id,
                    &package,
                    0,
                    active.scheme.card.max_minutes,
                    available,
                    false,
                ) {
                    if can_extend(
                        card.id,
                        card.id,
                        &package,
                        0,
                        active.scheme.card.max_minutes,
                        package.price_cents,
                        false,
                    ) {
                        return Err(SwipeError::CardBalance);
                    }
                    return Err(SwipeError::CardDeclined);
                }
                start_purchase_in_tx(
                    &mut *c,
                    self,
                    input,
                    station_id,
                    &port,
                    card.id,
                    card.user_id,
                    &openid,
                    active,
                    &package,
                    &operation_id,
                    &mut wallet,
                    now_micros,
                )
                .await?
            }
        };
        tx.commit().await?;
        Ok(CardOperationResult {
            operation_id,
            charge_order_id: order_id,
            kind,
            price_cents: package.price_cents,
            minutes: package.minutes.min(u16::MAX as u64) as u16,
            status,
            replayed: false,
        })
    }
}

/// 首购事务段：支付单/订单/意图/快照/会话/操作/出站事件（拆出避免主函数过长）。
/// 返回 (订单 ID, 套餐, 操作状态)。
#[allow(clippy::too_many_arguments)]
async fn start_purchase_in_tx(
    c: &mut sqlx::MySqlConnection,
    service: &SwipeService,
    input: &SwipeInput,
    station_id: u64,
    port: &chargepilot_mod_payment::scan::ScanPort,
    card_id: u64,
    user_id: u64,
    openid: &[u8],
    active: &chargepilot_mod_pricing::metering::ActiveRule,
    package: &chargepilot_domain::charging_scheme::Package,
    operation_id: &str,
    wallet: &mut chargepilot_mod_wallet::ledger::WalletRow,
    now_micros: i64,
) -> Result<
    (
        String,
        u64,
        chargepilot_domain::charging_scheme::Package,
        String,
    ),
    SwipeError,
> {
    // 引擎规格与快照（与 checkout 冻结同构；结算走 duration 分支按已购时长折算）。
    let engine_spec = active.scheme.spec_for(package);
    let estimate = Estimate {
        mode: ChargeMode::new(ChargeMode::DEVICE_DURATION),
        estimated_kwh: "0.000".to_string(),
        estimated_minutes: package.minutes.min(u16::MAX as u64) as u16,
        prepaid_cents: package.price_cents,
        total_cents: package.price_cents,
        charge_mode: 4,
        charge_quantity: active.scheme.card.max_minutes,
        ..Default::default()
    };
    let offer = Offer {
        server_duration: true,
        package_id: package.id,
        id: active.rule_id * 100 + package.id,
        station_id,
        name: package.name.clone(),
        mode: package.mode.clone(),
        price_cents: package.price_cents,
        duration_minutes: package.minutes.min(u16::MAX as u64) as u16,
        ..Default::default()
    };
    let snapshot = PricingSnapshot {
        rule: Rule {
            id: active.rule_id,
            station_id,
            version: active.version as u32,
            spec: engine_spec,
            ..Default::default()
        },
        estimate,
        offer,
        computed_at_unix_micros: now_micros,
    };
    let snapshot_json =
        serde_json::to_value(&snapshot).map_err(|e| SwipeError::Persist(e.to_string()))?;

    let payment_order_id = service
        .allocator
        .next_id()
        .await
        .map_err(SwipeError::from_anyhow)?;
    let pay_no = new_payment_order_no(&service.allocator)
        .await
        .map_err(map_order_error)?;
    create_card_payment_in_tx(c, payment_order_id, &pay_no, user_id, package.price_cents).await?;
    let order_no = new_charge_order_no_tx(c, now_micros, &resolved_device_id(port), port.port_no)
        .await
        .map_err(map_order_error)?;
    let charge_order_id = create_paid_in_tx(
        c,
        &PaidOrderDraft {
            order_no: order_no.clone(),
            user_id,
            device_id: resolved_device_id(port),
            port_no: port.port_no,
            port_code: port.port_id.clone(),
            payment_order_id,
            charge_mode: 4,
            charge_quantity: active.scheme.card.max_minutes,
            discount_cents: 0,
        },
    )
    .await
    .map_err(map_order_error)?;
    set_card_order_in_tx(c, payment_order_id, charge_order_id).await?;
    let intent_id = uuid_v4();
    create_card_intent_in_tx(
        c,
        &intent_id,
        operation_id,
        &pay_no,
        payment_order_id,
        user_id,
        openid,
        &resolved_device_id(port),
        port.port_no,
        &port.port_id,
        station_id,
        active.rule_id,
        active.version as u32,
        &snapshot_json,
        package.minutes.min(u16::MAX as u64) as u16,
        package.price_cents,
        active.scheme.card.max_minutes,
        charge_order_id,
    )
    .await?;
    freeze_pricing_in_tx(
        c,
        charge_order_id,
        &intent_id,
        user_id,
        &port.port_id,
        &snapshot_json,
    )
    .await
    .map_err(map_order_error)?;
    // 刷卡会话：active_port 唯一键串行同端口占用，冲突即回滚（旧版语义）。
    let inserted = sqlx::query(
        "INSERT INTO `card_charge` (`charge_order_id`, `card_id`, `port_code`, `active_port`, \
         `paid_cents`, `purchased_minutes`, `max_minutes`, `card_no`, `wallet_after_cents`, \
         `package_json`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS JSON))",
    )
    .bind(charge_order_id as i64)
    .bind(card_id as i64)
    .bind(&port.port_id)
    .bind(&port.port_id)
    .bind(package.price_cents)
    .bind(package.minutes as i64)
    .bind(active.scheme.card.max_minutes)
    .bind(&input.card_no)
    .bind(wallet.balance_cents - wallet.frozen_cents - package.price_cents)
    .bind(serde_json::to_string(package).map_err(|e| SwipeError::Persist(e.to_string()))?)
    .execute(&mut *c)
    .await;
    if let Err(e) = inserted {
        if let sqlx::Error::Database(db) = &e
            && db.code().as_deref() == Some("23000")
        {
            return Err(SwipeError::CardDeclined);
        }
        return Err(SwipeError::Persist(e.to_string()));
    }
    // 钱包扣款（出账要求 active 且可用充足；失败整体回滚）。
    move_tx(
        c,
        user_id,
        wallet,
        -package.price_cents,
        &format!("CP{operation_id}"),
        "在线刷卡start",
    )
    .await
    .map_err(map_wallet_error)?;
    // 操作记录（confirming：启动回执确认）。
    sqlx::query(
        "INSERT INTO `card_operation` (`operation_id`, `device_id`, `event_id`, \
         `charge_order_id`, `card_id`, `kind`, `price_cents`, `minutes`, `status`) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(operation_id)
    .bind(resolved_device_id(port))
    .bind(&input.event_id)
    .bind(charge_order_id as i64)
    .bind(card_id as i64)
    .bind(CardOperationKind::Start)
    .bind(package.price_cents)
    .bind(package.minutes as i64)
    .bind(CardOperationStatus::Confirming)
    .execute(&mut *c)
    .await?;
    // 订单事件 + 出站事件：驱动 StartDispatcher 下发启动命令（与支付链同构）。
    append_event_in_tx(
        c,
        charge_order_id,
        operation_id,
        "card_start_confirming",
        "card",
        &format!(
            "card={} cents={} minutes={}",
            card_id, package.price_cents, package.minutes
        ),
        now_micros,
    )
    .await
    .map_err(map_order_error)?;
    append_domain_event_tx(
        c,
        operation_id,
        "user",
        "charge_payment_confirmed",
        &serde_json::json!({
            "event_id": operation_id,
            "charge_order_id": charge_order_id,
            "order_no": order_no,
        }),
    )
    .await
    .map_err(map_order_error)?;
    Ok((
        CardOperationKind::Start.as_str().to_owned(),
        charge_order_id,
        package.clone(),
        CardOperationStatus::Confirming.as_str().to_owned(),
    ))
}

/// 端口所属设备号（ScanPort 携带）。
fn resolved_device_id(port: &chargepilot_mod_payment::scan::ScanPort) -> String {
    port.device_id.clone()
}

/// 钱包错误映射（旧版 mapWalletMoveError）。
// 钱包错误通配：非余额/未激活统一进存储错误（wildcard_enum_match_arm）。
#[allow(clippy::wildcard_enum_match_arm)]
fn map_wallet_error(e: chargepilot_mod_wallet::WalletError) -> SwipeError {
    match e {
        chargepilot_mod_wallet::WalletError::InsufficientBalance => SwipeError::CardBalance,
        chargepilot_mod_wallet::WalletError::Inactive => SwipeError::CardDeclined,
        other => SwipeError::Persist(other.to_string()),
    }
}

/// 订单域错误映射（冲突与存储统一为刷卡拒绝/存储错误）。
fn map_order_error(e: chargepilot_mod_order::OrderError) -> SwipeError {
    match e {
        chargepilot_mod_order::OrderError::NumberConflict => SwipeError::CardDeclined,
        chargepilot_mod_order::OrderError::Storage(m) => SwipeError::Persist(m),
    }
}

impl SwipeError {
    /// 号段分配错误（纯存储失败）。
    fn from_anyhow(e: anyhow::Error) -> SwipeError {
        SwipeError::Persist(e.to_string())
    }
}

/// 启动回执成功：首购操作 confirming → confirmed（旧版启动结果确认）。
pub async fn confirm_start_operation_tx(
    c: &mut sqlx::MySqlConnection,
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
    c: &mut sqlx::MySqlConnection,
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
    c: &mut sqlx::MySqlConnection,
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
