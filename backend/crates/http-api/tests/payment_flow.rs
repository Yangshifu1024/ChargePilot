//! 阶段 04 批 3 主缝集成测试：下单冻结（意图+快照+端口预占）、模拟确认
//! （支付成功事务：订单 paid + 快照冻结 + 事件出站）、迟到支付退款与
//! 钱包充值全链（批 2 缝合）。断言 HTTP 响应与库中资金行。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::{ApiDeps, WalletServices};
use chargepilot_platform::ids::SegmentAllocator;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .unwrap()
        })
        .clone()
}

/// 支付测试互斥锁：端口预占与频次窗口依赖库中唯一事实。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

type RouterEnv = (axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>);

async fn build() -> Option<RouterEnv> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return None;
    };
    let Some(redis_url) = env_or_skip("TEST_REDIS_URL") else {
        eprintln!("跳过：TEST_REDIS_URL 未设置");
        return None;
    };
    let db = match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("跳过：数据库连接失败 {e}");
            return None;
        }
    };
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 60));
    if let Err(e) = allocator.warm_up().await {
        eprintln!("跳过：号段预热失败 {e}");
        return None;
    }
    // 支付域按 simulation 装配（旧版 PAYMENT_MODE=simulation 语义）。
    let provider: Arc<dyn chargepilot_mod_payment::PayProvider> =
        Arc::new(chargepilot_mod_payment::channel::SimulationProvider);
    let scanner: Arc<dyn chargepilot_mod_payment::PortScanner> = Arc::new(
        chargepilot_mod_payment::scan::TablePortScanner::new(db.clone()),
    );
    let checkout = Arc::new(chargepilot_mod_payment::CheckoutService {
        pool: db.clone(),
        allocator: (*allocator).clone(),
        scanner: scanner.clone(),
        provider: provider.clone(),
    });
    let bridge = Arc::new(
        chargepilot_mod_payment::recharge_bridge::RechargePaymentBridge {
            pool: db.clone(),
            allocator: (*allocator).clone(),
            provider,
        },
    );
    let wallet_recharge = Arc::new(chargepilot_mod_wallet::recharge::RechargeService {
        db: db.clone(),
        payments: bridge,
    });
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: Some(Arc::new(WalletServices::build(db.clone()))),
        checkout: Some(checkout),
        wallet_recharge: Some(wallet_recharge),
        simulation_payment: true,
        snapshots: None,
        charging_stop: Some(Arc::new(chargepilot_mod_charging::StopService {
            pool: db.clone(),
            sink: Arc::new(chargepilot_mod_charging::SimulationSink),
        })),
    });
    Some((
        chargepilot_http_api::build_router(deps, prometheus_handle(), None).await,
        Arc::new(db),
        allocator,
    ))
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 合法 UUID v4（客户端幂等请求号）。
fn rand_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
}

async fn req(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path).header(
        "x-real-ip",
        format!("10.{}.{}.{}", rand_suffix(), rand_suffix(), rand_suffix()),
    );
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 开发登录并返回 (jwt, user_id)。
async fn dev_login(router: &axum::Router, openid: &str) -> (String, u64) {
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/auth/login",
        Some(json!({"code": format!("dev:{openid}")})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "开发登录应成功: {body}");
    (
        body["data"]["jwt"].as_str().unwrap().to_string(),
        body["data"]["user_id"].as_str().unwrap().parse().unwrap(),
    )
}

/// 种子数据：站点 + 厂商 + 设备 + 端口 + 计费规则，返回 (station_id, device_id, port_code, rule_id)。
async fn seed_station_device_rule(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> (u64, String, String, u64) {
    let station_id = alloc.next_id().await.unwrap();
    let vendor_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-p3-{}", rand_suffix());
    let port_code = format!("port-{}", rand_suffix());
    let rule_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '支付测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `status`) VALUES (?, ?, '支付测试厂商', 'dc589', 'enabled')")
        .bind(vendor_id as i64).bind(format!("v{}", rand_suffix())).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `station_id`, `status`) VALUES (?, ?, ?, 'enabled')")
        .bind(&device_id).bind(vendor_id as i64).bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 1, ?, 'idle')")
        .bind(&device_id).bind(&port_code).execute(db).await.unwrap();
    // 规则：电量模式单段全天，套餐 1 = 1 元金额档（spec 与 pricing_device_flow 一致）。
    let scheme = json!({
        "name": "支付测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [
            {"id": 1, "name": "1元", "mode": "amount", "price_cents": 100},
            {"id": 2, "name": "2小时", "mode": "duration", "price_cents": 200, "minutes": 120}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 2, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    let spec = json!({"scheme": scheme});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`) VALUES (?, '支付测试规则', ?, NULL, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    (station_id, device_id, port_code, rule_id)
}

/// 清理本测试的种子与订单（保留流水供追溯，只删维表与幂等表行）。
async fn cleanup(db: &sqlx::MySqlPool, device_id: &str) {
    let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `device` WHERE `device_id` = ?")
        .bind(device_id)
        .execute(db)
        .await;
}

#[tokio::test]
async fn checkout_confirm_and_late_refund_flow() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station_id, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;

    // 用户 A 登录（开发登录即建用户）。
    let openid_a = format!("pay-a-{}", rand_suffix());
    let (token_a, _user_a) = dev_login(&router, &openid_a).await;

    // 1. offers：端口套餐可查。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/offers",
        Some(json!({"port_code": port_code})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "offers 应成功: {body}");
    assert_eq!(
        body["data"]["items"].as_array().unwrap().len(),
        2,
        "应有两个套餐"
    );
    assert_eq!(body["data"]["stop_when_full"], true);

    // 2. checkout：冻结意图与端口。
    let request_id = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({"client_request_id": request_id, "port_code": port_code, "offer_id": rule_id * 100 + 1})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "下单应成功: {body}");
    let merchant_order_no = body["data"]["merchant_order_no"]
        .as_str()
        .unwrap()
        .to_string();
    let payable = body["data"]["payable_cents"].as_i64().unwrap();
    assert_eq!(payable, 100, "1 元金额档应付 100 分");
    assert!(
        body["data"]["payment_params"]["prepay_id"]
            .as_str()
            .unwrap()
            .starts_with("SIM"),
        "模拟渠道应返回 SIM 预支付编号"
    );
    let intent_id = body["data"]["intent_id"].as_str().unwrap().to_string();

    // 库断言：意图 initiated + 支付单 initiated + 快照冻结。
    let intent_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_payment_intent` WHERE `intent_id` = ?")
            .bind(&intent_id)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(intent_status, "initiated");
    let (pay_status, biz_id): (String, u64) =
        sqlx::query_as("SELECT `status`, `biz_id` FROM `payment_order` WHERE `order_no` = ?")
            .bind(&merchant_order_no)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(pay_status, "initiated");
    assert_eq!(biz_id, 0, "支付单在到账前不关联业务");

    // 3. 同请求重放：同一商户单号（幂等）。
    let (status, replay) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({"client_request_id": request_id, "port_code": port_code, "offer_id": rule_id * 100 + 1})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放应成功: {replay}");
    assert_eq!(
        replay["data"]["merchant_order_no"].as_str().unwrap(),
        merchant_order_no,
        "重放返回同一支付单"
    );

    // 4. 同请求不同套餐：409（旧版 existingIntent 逐项核对）。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({"client_request_id": request_id, "port_code": port_code, "offer_id": rule_id * 100 + 2})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "同请求不同套餐应冲突: {body}");

    // 5. 模拟确认：走支付成功事务。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/development/payments/confirm",
        Some(json!({"merchant_order_no": merchant_order_no})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "模拟确认应成功: {body}");
    assert_eq!(body["data"]["replayed"], false);
    let charge_order_id = body["data"]["charge_order_id"].as_u64().unwrap();
    let charge_order_no = body["data"]["charge_order_no"].as_str().unwrap();
    assert!(
        charge_order_no.starts_with('C'),
        "充电单号应为 C 前缀: {charge_order_no}"
    );

    // 库断言：订单 paid + 快照 + 支付单绑定 + 意图 paid + 事件出站。
    let (order_status, pay_status2): (String, String) = sqlx::query_as(
        "SELECT c.`status`, p.`status` FROM `charge_order` c \
         JOIN `payment_order` p ON p.`id` = c.`payment_order_id` WHERE c.`id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(order_status, "paid", "支付成功订单应为 paid");
    assert_eq!(pay_status2, "paid");
    let snapshot_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_order_pricing` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(snapshot_count, 1, "快照必须已冻结");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `domain_event` WHERE `kind` = 'charge_payment_confirmed'",
    )
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert!(event_count >= 1, "出站事件必须已写入");

    // 6. 二次确认：幂等重放。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/development/payments/confirm",
        Some(json!({"merchant_order_no": merchant_order_no})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "二次确认应成功: {body}");
    assert_eq!(body["data"]["replayed"], true);
    assert_eq!(
        body["data"]["charge_order_id"].as_u64().unwrap(),
        charge_order_id
    );

    // 7. 端口预占：用户 B 同端口下单被拒（端口已被进行中订单占用）。
    let openid_b = format!("pay-b-{}", rand_suffix());
    let (token_b, _user_b) = dev_login(&router, &openid_b).await;
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({"client_request_id": rand_uuid(), "port_code": port_code, "offer_id": rule_id * 100 + 1})),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "已占端口应拒绝: {body}");

    // 8. 迟到支付：B 下单后把支付期限改到过去，确认后只登记退款。
    let port_code2 = format!("port-{}", rand_suffix());
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 2, ?, 'idle')")
        .bind(&device_id).bind(&port_code2).execute(db.as_ref()).await.unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({"client_request_id": rand_uuid(), "port_code": port_code2, "offer_id": rule_id * 100 + 1})),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "B 下单应成功: {body}");
    let late_order_no = body["data"]["merchant_order_no"]
        .as_str()
        .unwrap()
        .to_string();
    sqlx::query("UPDATE `charge_payment_intent` SET `expires_at` = DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE) WHERE `merchant_order_no` = ?")
        .bind(&late_order_no)
        .execute(db.as_ref())
        .await
        .unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/development/payments/confirm",
        Some(json!({"merchant_order_no": late_order_no})),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "迟到确认应成功并登记退款: {body}");
    assert_eq!(body["data"]["refund_required"], true);
    assert_eq!(body["data"]["charge_order_id"], 0, "迟到支付不创建充电订单");
    let refund_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `refund_record` r JOIN `payment_order` p ON p.`id` = r.`payment_order_id` \
         WHERE p.`order_no` = ? AND r.`status` = 'pending' AND r.`execution_policy` = 'automatic'",
    )
    .bind(&late_order_no)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(refund_count, 1, "迟到退款记录必须登记");

    cleanup(db.as_ref(), &device_id).await;
}

#[tokio::test]
async fn wallet_recharge_full_chain() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, _alloc)) = build().await else {
        return;
    };
    // 充值链不需要设备；直接登录用户。
    let openid = format!("pay-w-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;

    // 1. 充值申请：幂等请求号 + 500 分。
    let request_id = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/recharge",
        Some(json!({"request_id": request_id, "amount_cents": 500})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "充值申请应成功: {body}");
    assert_eq!(body["data"]["can_pay"], true);
    let merchant_order_no = body["data"]["merchant_order_no"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. 同请求重放：同支付单。
    let (status, replay) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/recharge",
        Some(json!({"request_id": request_id, "amount_cents": 500})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "充值重放应成功: {replay}");
    assert_eq!(
        replay["data"]["merchant_order_no"].as_str().unwrap(),
        merchant_order_no
    );

    // 3. 模拟确认 → 余额到账（批 2 缝合：支付成功事务 → 钱包入账）。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/development/payments/confirm",
        Some(json!({"merchant_order_no": merchant_order_no})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "充值确认应成功: {body}");
    assert_eq!(body["data"]["replayed"], false);

    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance, 500, "余额应到账 500 分");
    let txn_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `wallet_txn` WHERE `user_id` = ? AND `direction` = 'in'",
    )
    .bind(user_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(txn_count, 1, "入账流水一条");

    // 4. 二次确认：不重复入账。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/development/payments/confirm",
        Some(json!({"merchant_order_no": merchant_order_no})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "充值重放确认应成功");
    let balance2: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance2, 500, "重放不得重复入账");

    // 5. C 端余额接口可见（批 2 路由缝合）。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/balance",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "余额查询应成功: {body}");
    assert_eq!(body["data"]["balance_cents"], 500);
}

/// 批 4 互斥约束：套餐下单带券返回冲突（旧版 Reserve 的
/// "当前套餐不允许使用优惠券"）；无券字段正常下单不受影响。
#[tokio::test]
async fn checkout_rejects_coupon_with_offer() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station_id, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("pay-mk-{}", rand_suffix());
    let (token, _uid) = dev_login(&router, &openid).await;
    // 带 coupon_grant_id 的下单：互斥冲突（校验在扫码读取之前，端口不存在也不影响）。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/start",
        Some(json!({
            "client_request_id": rand_uuid(), "port_code": port_code,
            "offer_id": rule_id * 100 + 1, "coupon_grant_id": "12345"
        })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "套餐+券应互斥: {body}");
    cleanup(&db, &device_id).await;
}
