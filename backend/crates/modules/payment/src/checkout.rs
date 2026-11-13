//! 下单与支付准备（旧版 CheckoutService 对齐）：套餐查询、支付意图冻结与
//! 预支付参数。下单即冻结计费快照（红线层语义照搬）；预支付失败保留申请，
//! 相同 client_request_id 可原单号恢复。

use super::channel::PrepayRequest;
use super::scan::{ScanError, ScanResult};
use chargepilot_domain::charging_actual::Rule;
use chargepilot_domain::charging_settle::{Estimate, Offer};
use chargepilot_domain::money::{PayMethod, PaymentBizType, PaymentStatus};
use chargepilot_domain::payment_intent::{IntentError, IntentStatus, intent_estimate};
use chargepilot_platform::ids::SegmentAllocator;
use sqlx::{Acquire, MySqlPool};

/// 下单域错误（HTTP 层映射旧版 CheckoutError 的 Kind 族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutError {
    /// 输入不合法。
    InvalidInput,
    /// 端口或设备不存在。
    NotFound,
    /// 设备已停用。
    Paused,
    /// 端口不可用（被预占或非空闲）。
    PortBusy,
    /// 套餐不可售。
    OfferUnavailable,
    /// 计费方案不可用或与设备能力不兼容。
    Pricing,
    /// 支付申请冲突（并发或重放不一致）。
    Conflict,
    /// 渠道不可用。
    Provider,
    /// 存储错误。
    Storage(String),
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            CheckoutError::InvalidInput => "请求参数无效",
            CheckoutError::NotFound => "设备或端口不存在",
            CheckoutError::Paused => "设备已停用",
            CheckoutError::PortBusy => "端口已被占用",
            CheckoutError::OfferUnavailable => "套餐不可售",
            CheckoutError::Pricing => "计费方案不可用",
            CheckoutError::Conflict => "支付申请冲突",
            CheckoutError::Provider => "支付渠道不可用",
            CheckoutError::Storage(m) => return write!(f, "下单存储错误: {m}"),
        };
        f.write_str(text)
    }
}

impl std::error::Error for CheckoutError {}

impl From<sqlx::Error> for CheckoutError {
    fn from(e: sqlx::Error) -> Self {
        CheckoutError::Storage(e.to_string())
    }
}

impl From<ScanError> for CheckoutError {
    fn from(e: ScanError) -> Self {
        match e {
            ScanError::NotFound => CheckoutError::NotFound,
            ScanError::Gateway(m) => CheckoutError::Storage(m),
        }
    }
}

impl From<IntentError> for CheckoutError {
    fn from(e: IntentError) -> Self {
        match e {
            IntentError::Conflict => CheckoutError::Conflict,
            IntentError::OfferUnavailable => CheckoutError::OfferUnavailable,
            IntentError::InvalidPricing => CheckoutError::Pricing,
        }
    }
}

/// 端口当前套餐与展示配置（旧版 CheckoutOffers）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckoutOffers {
    pub station_id: u64,
    pub port_id: String,
    /// 在售套餐（旧版 Offers）。
    pub items: Vec<Offer>,
    /// 用户端呈现配置。
    pub display: chargepilot_domain::charging_scheme::Display,
    /// 是否"充满自停"。
    pub stop_when_full: bool,
}

/// 下单请求（旧版 CheckoutInput）。
#[derive(Debug, Clone)]
pub struct CheckoutInput {
    /// 客户端幂等请求 ID（UUID）。
    pub client_request_id: String,
    /// 端口二维码编码。
    pub port_code: String,
    /// 在售套餐 ID（rule.id*100+package.id）。
    pub offer_id: u64,
    /// 券发放 ID。当前套餐下单不支持优惠券（旧版 Reserve 互斥约束：
    /// "当前套餐不允许使用优惠券"，非 0 返回冲突）；报价与核销见
    /// chargepilot-mod-marketing 的 quote/redeem_in_tx。
    pub coupon_grant_id: u64,
}

/// 支付准备响应（旧版 CheckoutResult；不含内部支付主键与 openid）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckoutResult {
    pub intent_id: String,
    pub merchant_order_no: String,
    pub port_id: String,
    pub device_id: String,
    pub station_id: u64,
    /// 预估电费，分（金额/时长/电量套餐为 0）。
    pub electric_cents: i64,
    pub service_cents: i64,
    /// 优惠前总额，分。
    pub total_cents: i64,
    /// 已冻结优惠额，分（本阶段恒 0，阶段 05 接入券）。
    pub discount_cents: i64,
    /// 应付金额，分。
    pub payable_cents: i64,
    pub coupon_grant_id: u64,
    /// 渠道支付参数（重试复用保存结果）。
    pub payment_params: serde_json::Value,
    /// 支付期限（UTC 微秒）。
    pub expires_at_unix_micros: i64,
}

/// 已冻结的支付意图事实（reserve/replay 的返回）。
#[derive(Debug, Clone)]
pub struct FrozenIntent {
    pub intent_id: String,
    pub merchant_order_no: String,
    pub payment_order_id: u64,
    pub user_id: u64,
    pub device_id: String,
    pub port_no: u8,
    pub port_code: String,
    pub station_id: u64,
    pub estimate: Estimate,
    pub discount_cents: i64,
    pub payable_cents: i64,
    pub expires_at_unix_micros: i64,
}

/// 冻结快照的信封（pricing_snapshot JSON 的结构；settlement 批次只读）。
/// rule.spec 为选中套餐的引擎规格（旧版 SpecFor 替换语义）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PricingSnapshot {
    /// 当前生效方案及规则版本（首次创建冻结）。
    pub rule: Rule,
    /// 首次冻结的费用及设备执行量。
    pub estimate: Estimate,
    /// 后台发布的套餐。
    pub offer: Offer,
    /// 冻结时刻（UTC 微秒）。
    pub computed_at_unix_micros: i64,
}

/// 下单服务（旧版 CheckoutService）。依赖经 cmd 装配注入。
pub struct CheckoutService {
    pub pool: MySqlPool,
    pub allocator: SegmentAllocator,
    pub scanner: std::sync::Arc<dyn super::scan::PortScanner>,
    pub provider: std::sync::Arc<dyn super::channel::PayProvider>,
}

impl CheckoutService {
    /// 返回端口的当前套餐和展示配置；无需登录，不创建支付或订单（旧版 Offers）。
    pub async fn offers(&self, port_code: &str) -> Result<CheckoutOffers, CheckoutError> {
        let port = self.scan_port(port_code).await?;
        if port.device_status != "enabled" {
            return Err(CheckoutError::Paused);
        }
        let active = self.active_rule(&port).await?;
        let stub = rule_stub(&active, &port);
        Ok(CheckoutOffers {
            station_id: port.station_id,
            port_id: port_code.to_string(),
            items: active.scheme.offers(&stub),
            display: active.scheme.clone().normalized().display,
            stop_when_full: active.scheme.clone().normalized().stop.stop_when_full,
        })
    }

    /// 为用户准备付款：先恢复同请求的支付，再读取当前设备与方案（旧版 Start）。
    /// 支付申请提交后才请求预支付参数；失败保留申请供原单号重试。
    pub async fn start(
        &self,
        user_id: u64,
        input: &CheckoutInput,
    ) -> Result<CheckoutResult, CheckoutError> {
        if user_id == 0
            || !is_uuid(&input.client_request_id)
            || input.port_code.is_empty()
            || input.offer_id == 0
        {
            return Err(CheckoutError::InvalidInput);
        }
        // 套餐下单不支持优惠券（旧版 Reserve 的互斥约束）；quote/redeem 服务
        // 已在 marketing 模块就位，非套餐支付路径开放时接入。
        if input.coupon_grant_id != 0 {
            return Err(CheckoutError::Conflict);
        }
        // 同请求幂等恢复：已存在意图则直接重放预支付。
        if let Some(intent) = self.replay(user_id, input).await? {
            return self.prepare(intent).await;
        }
        let port = self.scan_port(&input.port_code).await?;
        if port.device_status != "enabled" {
            return Err(CheckoutError::Paused);
        }
        let scan_port = port.port.clone().ok_or(CheckoutError::PortBusy)?;
        if !scan_port.available || port.station_id == 0 {
            return Err(CheckoutError::PortBusy);
        }
        let active = self.active_rule(&port).await?;
        let selected = active
            .scheme
            .offers(&rule_stub(&active, &port))
            .into_iter()
            .find(|o| o.id == input.offer_id)
            .ok_or(CheckoutError::OfferUnavailable)?;
        let package = active
            .scheme
            .clone()
            .normalized()
            .packages
            .into_iter()
            .find(|p| p.id == selected.package_id)
            .ok_or(CheckoutError::OfferUnavailable)?;
        let estimate = intent_estimate(&active.scheme, &package, selected.price_cents)?;
        let intent = self
            .reserve(
                user_id, input, &port, &active, &selected, &package, &estimate,
            )
            .await?;
        self.prepare(intent).await
    }

    /// 按扫码编码读取端口事实（旧版 port；NotFound 与网关故障区分）。
    async fn scan_port(&self, code: &str) -> Result<ScanResult, CheckoutError> {
        Ok(self.scanner.resolve_scan(code).await?)
    }

    /// 读取站点/设备生效规则（旧版 ActiveDeviceRule）。
    async fn active_rule(
        &self,
        port: &ScanResult,
    ) -> Result<chargepilot_mod_pricing::metering::ActiveRule, CheckoutError> {
        chargepilot_mod_pricing::metering::active_device_rule(
            &self.pool,
            port.station_id,
            Some(&port.device_id),
        )
        .await
        .map_err(|_| CheckoutError::Pricing)
    }

    /// 同（user, request）的意图恢复：参数一致返回冻结事实，不一致报冲突
    /// （旧版 Replay + existingIntent 的逐项核对）。
    async fn replay(
        &self,
        user_id: u64,
        input: &CheckoutInput,
    ) -> Result<Option<FrozenIntent>, CheckoutError> {
        let row: Option<(
            String,
            String,
            u64,
            String,
            u8,
            u64,
            i64,
            i64,
            i64,
            i64,
            u16,
            u16,
            u64,
            u64,
            chrono::DateTime<chrono::Utc>,
            serde_json::Value,
        )> = sqlx::query_as(
            "SELECT CAST(i.`intent_id` AS CHAR), i.`merchant_order_no`, i.`station_id`, i.`device_id`, \
                    i.`port_no`, i.`offer_id`, i.`discount_cents`, i.`electric_cents`, \
                    i.`service_cents`, i.`total_cents`, i.`estimated_minutes`, \
                    i.`charge_quantity`, i.`payment_order_id`, CAST(COALESCE(i.`coupon_grant_id`, 0) AS UNSIGNED), \
                    i.`expires_at`, i.`pricing_snapshot` \
             FROM `charge_payment_intent` i \
             WHERE i.`user_id` = ? AND i.`client_request_id` = ? AND i.`status` = ? LIMIT 1",
        )
        .bind(user_id as i64)
        .bind(&input.client_request_id)
        .bind(IntentStatus::INITIATED)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (
            intent_id,
            merchant_order_no,
            station_id,
            device_id,
            port_no,
            offer_id,
            discount_cents,
            electric_cents,
            service_cents,
            total_cents,
            estimated_minutes,
            charge_quantity,
            payment_order_id,
            intent_coupon_id,
            expires_at,
            snapshot_value,
        ) = row;
        // 重放必须与请求同端口同套餐同券（旧版逐项核对；不使用当前规则替换快照）。
        let port = self.scan_port(&input.port_code).await?;
        if port.device_id != device_id || offer_id != input.offer_id {
            return Err(CheckoutError::Conflict);
        }
        // 重放必须使用首次意图绑定的优惠券（旧版 CouponGrantID 核对）。
        if intent_coupon_id != input.coupon_grant_id {
            return Err(CheckoutError::Conflict);
        }
        let snapshot: PricingSnapshot = serde_json::from_value(snapshot_value)
            .map_err(|e| CheckoutError::Storage(e.to_string()))?;
        Ok(Some(FrozenIntent {
            intent_id,
            merchant_order_no,
            payment_order_id,
            user_id,
            device_id,
            port_no,
            port_code: input.port_code.clone(),
            station_id,
            estimate: Estimate {
                estimated_kwh: snapshot.estimate.estimated_kwh.clone(),
                estimated_minutes,
                mode: snapshot.estimate.mode.clone(),
                basis: String::new(),
                electric_cents,
                service_cents,
                total_cents,
                prepaid_cents: total_cents,
                charge_mode: snapshot.estimate.charge_mode,
                charge_quantity,
            },
            discount_cents,
            payable_cents: total_cents - discount_cents,
            expires_at_unix_micros: expires_at.timestamp_micros(),
        }))
    }

    /// 冻结意图事务（旧版 Reserve）：READ COMMITTED，同事务完成
    /// 过期预占释放 → 端口可用性 → openid → 支付单 → 意图。
    #[allow(clippy::too_many_arguments)]
    async fn reserve(
        &self,
        user_id: u64,
        input: &CheckoutInput,
        port: &ScanResult,
        active: &chargepilot_mod_pricing::metering::ActiveRule,
        offer: &Offer,
        package: &chargepilot_domain::charging_scheme::Package,
        estimate: &Estimate,
    ) -> Result<FrozenIntent, CheckoutError> {
        let scan_port = port.port.clone().ok_or(CheckoutError::PortBusy)?;
        let intent_id = new_uuid_v4();
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        // 快照 rule.spec 为选中套餐的引擎规格（旧版 SpecFor 替换语义）。
        let snapshot = PricingSnapshot {
            rule: Rule {
                id: active.rule_id,
                station_id: port.station_id,
                device_id: port.device_id.clone(),
                version: active.version as u32,
                spec: active.scheme.spec_for(package),
                channel: String::new(),
            },
            estimate: estimate.clone(),
            offer: offer.clone(),
            computed_at_unix_micros: chrono::Utc::now().timestamp_micros(),
        };
        let snapshot_json =
            serde_json::to_string(&snapshot).map_err(|e| CheckoutError::Storage(e.to_string()))?;
        let merchant_order_no = chargepilot_mod_order::new_payment_order_no(&self.allocator)
            .await
            .map_err(|e| CheckoutError::Storage(e.to_string()))?;

        // 先设隔离级别再开事务（READ COMMITTED：等锁后的重读能看到前次提交）。
        let mut conn = self.pool.acquire().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin().await?;
        let conn = tx.acquire().await?;

        // 仅释放本端口已过期的预占，与新意图创建同事务（旧版语义）。
        sqlx::query(
            "UPDATE `charge_payment_intent` SET `status` = ? \
             WHERE `port_code` = ? AND `status` = ? AND `expires_at` <= UTC_TIMESTAMP(3)",
        )
        .bind(IntentStatus::EXPIRED)
        .bind(&scan_port.port_id)
        .bind(IntentStatus::INITIATED)
        .execute(&mut *conn)
        .await?;
        // 端口可用性：有效预占或进行中订单存在即冲突（旧版 CheckoutPortAvailable）。
        let busy: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM `charge_payment_intent` i \
             LEFT JOIN `charge_order` c ON c.`id` = i.`charge_order_id` \
             WHERE i.`port_code` = ? AND ((i.`status` = ? AND i.`expires_at` > UTC_TIMESTAMP(3)) \
               OR (i.`status` = ? AND c.`status` IN (?, ?)))",
        )
        .bind(&scan_port.port_id)
        .bind(IntentStatus::INITIATED)
        .bind(IntentStatus::PAID)
        .bind(chargepilot_mod_order::ONGOING_STATUSES[0])
        .bind(chargepilot_mod_order::ONGOING_STATUSES[1])
        .fetch_one(&mut *conn)
        .await?;
        if busy != 0 {
            return Err(CheckoutError::PortBusy);
        }
        // 付款人微信身份（不公开；varbinary 列读取侧 CAST AS CHAR）。
        let openid: Option<String> = sqlx::query_scalar(
            "SELECT CAST(`openid` AS CHAR) FROM `user` \
             WHERE `id` = ? AND `status` = 'active' AND `deleted_at` IS NULL LIMIT 1",
        )
        .bind(user_id as i64)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        let Some(openid) = openid else {
            return Err(CheckoutError::Conflict);
        };
        // 套餐下单不支持优惠券（旧版互斥约束，start 已拦截）：优惠额恒 0；
        // 报价与到账核销见 marketing::coupon 的 quote/redeem_in_tx。
        let discount_cents = 0i64;
        let payable = estimate.total_cents - discount_cents;
        let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
        let payment_id: u64 = sqlx::query(
            "INSERT INTO `payment_order` \
             (`order_no`, `biz_type`, `biz_id`, `user_id`, `pay_method`, `total_cents`, \
              `status`, `expired_at`, `created_month`) \
             VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&merchant_order_no)
        .bind(PaymentBizType::CHARGE)
        .bind(user_id as i64)
        .bind(PayMethod::WECHAT)
        .bind(payable)
        .bind(PaymentStatus::INITIATED)
        .bind(expires_at)
        .bind(month)
        .execute(&mut *conn)
        .await?
        .last_insert_id() as u64;
        let inserted = sqlx::query(
            "INSERT INTO `charge_payment_intent` \
             (`intent_id`, `client_request_id`, `merchant_order_no`, `payment_order_id`, `user_id`, \
              `openid`, `device_id`, `port_no`, `port_code`, `station_id`, `pricing_rule_id`, \
              `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, `estimated_minutes`, \
              `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, `charge_quantity`, \
              `status`, `expires_at`, `offer_id`) \
             VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&intent_id)
        .bind(&input.client_request_id)
        .bind(&merchant_order_no)
        .bind(payment_id as i64)
        .bind(user_id as i64)
        .bind(&openid)
        .bind(&port.device_id)
        .bind(scan_port.port_no)
        .bind(&scan_port.port_id)
        .bind(port.station_id as i64)
        .bind(active.rule_id as i64)
        .bind(active.version as i64)
        .bind(&snapshot_json)
        .bind(&estimate.estimated_kwh)
        .bind(estimate.estimated_minutes as i64)
        .bind(estimate.electric_cents)
        .bind(estimate.service_cents)
        .bind(estimate.total_cents)
        .bind(estimate.charge_mode)
        .bind(estimate.charge_quantity as i64)
        .bind(IntentStatus::INITIATED)
        .bind(expires_at)
        .bind(offer.id as i64)
        .execute(&mut *conn)
        .await;
        match inserted {
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23000") => {
                return Err(CheckoutError::Conflict);
            }
            Err(e) => return Err(CheckoutError::from(e)),
            Ok(_) => {}
        }
        tx.commit().await?;
        Ok(FrozenIntent {
            intent_id,
            merchant_order_no,
            payment_order_id: payment_id,
            user_id,
            device_id: port.device_id.clone(),
            port_no: scan_port.port_no,
            port_code: scan_port.port_id,
            station_id: port.station_id,
            estimate: estimate.clone(),
            discount_cents,
            payable_cents: payable,
            expires_at_unix_micros: expires_at.timestamp_micros(),
        })
    }

    /// 为意图复用或首写预支付参数；失败不撤销已提交申请（旧版 prepare）。
    /// 保存后重读首次参数，避免并发请求返回不同的支付参数。
    async fn prepare(&self, intent: FrozenIntent) -> Result<CheckoutResult, CheckoutError> {
        let params_json = match sqlx::query_scalar::<_, String>(
            "SELECT CAST(`params_json` AS CHAR) FROM `charge_prepay` WHERE `payment_order_id` = ?",
        )
        .bind(intent.payment_order_id as i64)
        .fetch_optional(&self.pool)
        .await?
        {
            Some(saved) => saved,
            None => {
                let params = self
                    .provider
                    .prepay(PrepayRequest {
                        merchant_order_no: intent.merchant_order_no.clone(),
                        open_id: String::new(),
                        amount_cents: intent.payable_cents,
                        expires_at_unix_micros: intent.expires_at_unix_micros,
                    })
                    .await
                    .map_err(|_| CheckoutError::Provider)?;
                if params.is_empty() {
                    return Err(CheckoutError::Provider);
                }
                let body = serde_json::to_string(&params)
                    .map_err(|e| CheckoutError::Storage(e.to_string()))?;
                let saved = sqlx::query(
                    "INSERT IGNORE INTO `charge_prepay` (`payment_order_id`, `params_json`, `prepay_id`) \
                     VALUES (?, CAST(? AS JSON), ?)",
                )
                .bind(intent.payment_order_id as i64)
                .bind(&body)
                .bind(&params.prepay_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
                if saved == 1 {
                    body
                } else {
                    // 并发者已保存：以先到者为准（旧版重读语义）。
                    sqlx::query_scalar::<_, String>(
                        "SELECT CAST(`params_json` AS CHAR) FROM `charge_prepay` WHERE `payment_order_id` = ?",
                    )
                    .bind(intent.payment_order_id as i64)
                    .fetch_one(&self.pool)
                    .await?
                }
            }
        };
        Ok(CheckoutResult {
            intent_id: intent.intent_id,
            merchant_order_no: intent.merchant_order_no,
            port_id: intent.port_code,
            device_id: intent.device_id,
            station_id: intent.station_id,
            electric_cents: intent.estimate.electric_cents,
            service_cents: intent.estimate.service_cents,
            total_cents: intent.estimate.total_cents,
            discount_cents: intent.discount_cents,
            payable_cents: intent.payable_cents,
            coupon_grant_id: 0,
            payment_params: serde_json::from_str(&params_json)
                .map_err(|e| CheckoutError::Storage(e.to_string()))?,
            expires_at_unix_micros: intent.expires_at_unix_micros,
        })
    }
}

/// ActiveRule → 快照外的临时 Rule（offers 只用编号与归属；spec 为空默认）。
fn rule_stub(active: &chargepilot_mod_pricing::metering::ActiveRule, port: &ScanResult) -> Rule {
    Rule {
        id: active.rule_id,
        station_id: port.station_id,
        device_id: port.device_id.clone(),
        version: active.version as u32,
        spec: chargepilot_domain::charging_spec::Spec::default(),
        channel: String::new(),
    }
}

/// UUID v4 生成（转 platform::ids，命令/意图/请求共用实现）。
pub fn new_uuid_v4() -> String {
    chargepilot_platform::ids::uuid_v4()
}

/// 宽松 UUID 校验（8-4-4-4-12 十六进制；旧版 uuid.Validate 的等价判定）。
pub fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}
