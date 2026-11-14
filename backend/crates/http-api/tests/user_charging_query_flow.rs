//! 用户端充电查询族与扫码端点真库集成测试：历史/详情、充电中与快照回退、
//! 曲线窗口、扫码解析与端口查询、旧形状停止。对照小程序 formatOrder /
//! scan.js / curve.js 消费字段与旧版响应形状断言。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::{ApiDeps, WalletServices};
use chargepilot_mod_charging::{SimulationSink, StartDispatcher};
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

/// 状态机测试互斥锁：端口预占依赖库中唯一事实（与 charging_flow 同锁）。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn build() -> Option<(axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>)> {
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
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: Some(Arc::new(WalletServices::build(db.clone()))),
        checkout: None,
        wallet_recharge: None,
        simulation_payment: true,
        charging_stop: Some(Arc::new(chargepilot_mod_charging::StopService {
            pool: db.clone(),
            sink: Arc::new(SimulationSink),
        })),
        snapshots: Some(Arc::new(chargepilot_gateway_runtime::SnapshotStore::new())),
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

/// 种子：站点 + 设备 + 端口 + 规则 + 中央运营态（scan 需 device_meta）。
async fn seed_station_device_rule(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> (u64, String, String, u64) {
    let station_id = alloc.next_id().await.unwrap();
    let vendor_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-uq-{}", rand_suffix());
    let port_code = format!("port-{}", rand_suffix());
    let rule_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '查询族测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `status`) VALUES (?, ?, '查询族测试厂商', 'dc589', 'enabled')")
        .bind(vendor_id as i64).bind(format!("v{}", rand_suffix())).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `station_id`, `status`) VALUES (?, ?, ?, 'enabled')")
        .bind(&device_id).bind(vendor_id as i64).bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 1, ?, 'idle')")
        .bind(&device_id).bind(&port_code).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_meta` (`device_id`, `station_id`, `vendor_id`, `status`) VALUES (?, ?, ?, 'enabled')")
        .bind(&device_id).bind(station_id as i64).bind(vendor_id as i64).execute(db).await.unwrap();
    let scheme = json!({
        "name": "查询族测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    let spec = json!({"scheme": scheme});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`) VALUES (?, '查询族测试规则', ?, NULL, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    (station_id, device_id, port_code, rule_id)
}

/// 追加端口 2（同测试的第二单与第一单同秒同设备时避免单号冲突）。
async fn seed_extra_port(db: &sqlx::MySqlPool, device_id: &str) -> String {
    let port_code = format!("port-{}", rand_suffix());
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 2, ?, 'idle')")
        .bind(device_id).bind(&port_code).execute(db).await.unwrap();
    port_code
}

/// 创建已支付充电订单（同 charging_flow：SQL 直插意图与支付单 → apply_payment）。
#[allow(clippy::too_many_arguments)]
async fn create_paid_order(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    user_id: u64,
    device_id: &str,
    port_code: &str,
    port_no: u8,
    rule_id: u64,
) -> (u64, String, String) {
    let payment_id = alloc.next_id().await.unwrap();
    let intent_id = rand_uuid();
    let merchant_order_no = format!("P{}", payment_id);
    let order_no_hint = rand_uuid();
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, \
         `pay_method`, `total_cents`, `status`, `created_month`) \
         VALUES (?, ?, 'charge', 0, ?, 'wechat', 100, 'initiated', ?)",
    )
    .bind(payment_id as i64)
    .bind(&merchant_order_no)
    .bind(user_id as i64)
    .bind(chargepilot_mod_order::month_start_of(chrono::Utc::now()))
    .execute(db)
    .await
    .unwrap();
    let snapshot = json!({
        "rule": {"rule_id": rule_id, "station_id": 0, "version": 1,
                 "spec": {"mode": "server_energy",
                     "electric": {"basis": "energy", "periods": [
                         {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]}}},
        "estimate": {"estimated_kwh": "0.000", "estimated_minutes": 600,
                     "electric_cents": 0, "service_cents": 0, "total_cents": 100,
                     "charge_mode": 4, "charge_quantity": 600},
        "offer": {"id": rule_id * 100 + 1, "package_id": 1, "name": "1元",
                  "mode": "amount", "price_cents": 100},
        "computed_at_unix_micros": chrono::Utc::now().timestamp_micros()
    });
    let expires = chrono::Utc::now() + chrono::Duration::minutes(5);
    sqlx::query(
        "INSERT INTO `charge_payment_intent` (`intent_id`, `client_request_id`, `merchant_order_no`, \
         `payment_order_id`, `user_id`, `openid`, `device_id`, `port_no`, `port_code`, `station_id`, \
         `pricing_rule_id`, `pricing_rule_version`, `pricing_snapshot`, `estimated_kwh`, \
         `estimated_minutes`, `electric_cents`, `service_cents`, `total_cents`, `charge_mode`, \
         `charge_quantity`, `status`, `expires_at`, `offer_id`) \
         VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, ?, ?, 0, ?, 1, CAST(? AS JSON), '0.000', \
         600, 0, 0, 100, 4, 600, 'initiated', ?, ?)",
    )
    .bind(&intent_id)
    .bind(&order_no_hint)
    .bind(&merchant_order_no)
    .bind(payment_id as i64)
    .bind(user_id as i64)
    .bind(format!("sim-{}", user_id))
    .bind(device_id)
    .bind(port_no)
    .bind(port_code)
    .bind(rule_id as i64)
    .bind(serde_json::to_string(&snapshot).unwrap())
    .bind(expires)
    .bind((rule_id * 100 + 1) as i64)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO `charge_prepay` (`payment_order_id`, `params_json`, `prepay_id`) VALUES (?, CAST(? AS JSON), ?)")
        .bind(payment_id as i64)
        .bind(json!({"mode": "simulation"}).to_string())
        .bind(format!("SIM{merchant_order_no}"))
        .execute(db)
        .await
        .unwrap();
    let payment = chargepilot_mod_payment::apply::VerifiedPayment {
        merchant_order_no: merchant_order_no.clone(),
        transaction_id: format!("SIMH5{merchant_order_no}"),
        open_id: format!("sim-{user_id}"),
        paid_cents: 100,
        paid_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    let result = chargepilot_mod_payment::apply::apply_payment(db, &payment)
        .await
        .expect("支付成功事务应成功");
    assert_eq!(result.replayed, false);
    (
        result.charge_order_id,
        result.charge_order_no,
        merchant_order_no,
    )
}

/// 事件投递推进到 charging（同 charging_flow：启动命令 + simulation 闭环回执）。
async fn mark_charging(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    charge_order_id: u64,
    order_no: &str,
) {
    let dispatcher = StartDispatcher {
        pool: db.clone(),
        allocator: (*alloc).clone(),
        sink: Arc::new(SimulationSink),
        simulation: true,
    };
    let payload = json!({
        "event_id": format!("E{}", rand_suffix()),
        "charge_order_id": charge_order_id,
        "order_no": order_no,
    });
    dispatcher.handle(&payload).await.expect("启动投递应成功");
    let status: String = sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(status, "charging", "启动回执后订单应 charging");
}

/// 把订单标记为已完成并补齐费用回执、计费任务、计费截止与冻结展示快照。
async fn mark_completed_with_facts(db: &sqlx::MySqlPool, charge_order_id: u64, rule_id: u64) {
    sqlx::query(
        "UPDATE `charge_order` SET `status` = 'completed', \
         `started_at` = DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 10 MINUTE), \
         `ended_at` = UTC_TIMESTAMP(3), `charged_kwh` = 1.2345, `charged_seconds` = 600, \
         `electric_cents` = 50, `service_cents` = 30, `total_cents` = 80 \
         WHERE `id` = ?",
    )
    .bind(charge_order_id as i64)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `charge_fee_receipt` (`charge_order_id`, `calculation_no`, `result_json`, `shortfall_cents`) \
         VALUES (?, ?, CAST(? AS JSON), 0)",
    )
    .bind(charge_order_id as i64)
    .bind(format!("B{charge_order_id}{}", rand_suffix()))
    .bind(json!({
        "calculation_no": "B-test",
        "electric_cents": 50,
        "service_cents": 30,
        "total_cents": 80
    })
    .to_string())
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `charge_billing_job` (`charge_order_id`, `status`) VALUES (?, 'done')",
    )
    .bind(charge_order_id as i64)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO `charge_billing_cutoff` (`charge_order_id`, `cutoff_at`, `reason`) \
         VALUES (?, DATE_SUB(UTC_TIMESTAMP(3), INTERVAL 5 MINUTE), 'stop_request')",
    )
    .bind(charge_order_id as i64)
    .execute(db)
    .await
    .unwrap();
    // 冻结展示快照：scheme（display/amount/remark）与顶层 offer。
    let snapshot = json!({
        "rule": {"rule_id": rule_id, "station_id": 0, "version": 1,
                 "spec": {"mode": "server_energy",
                     "scheme": {
                         "name": "查询族方案", "remark": "充值前后规则不变",
                         "amount": {"algorithm": "server_energy", "periods": [
                             {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
                         "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
                         "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600,
                                    "loss_rate_bp": 0, "channel_bp": 10000},
                         "stop": {"stop_when_full": true},
                         "card": {"package_id": 1, "max_minutes": 600},
                         "display": {"duration": [{"label": "功率", "value": "3000W"}], "energy": []}
                     },
                     "electric": {"basis": "energy", "periods": [
                         {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]}}},
        "offer": {"id": rule_id * 100 + 1, "package_id": 1, "name": "1元",
                  "mode": "amount", "price_cents": 100},
        "computed_at_unix_micros": chrono::Utc::now().timestamp_micros()
    });
    sqlx::query(
        "UPDATE `charge_order_pricing` SET `pricing_snapshot` = CAST(? AS JSON) WHERE `charge_order_id` = ?",
    )
    .bind(snapshot.to_string())
    .bind(charge_order_id as i64)
    .execute(db)
    .await
    .unwrap();
}

/// 场景 1：已完订单的历史列表与详情字段（对照 formatOrder 消费字段）。
#[tokio::test]
async fn history_and_detail_fields() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("uq-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;
    let (order_id, order_no, merchant_no) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code, 1, rule_id).await;
    mark_completed_with_facts(&db, order_id, rule_id).await;

    // 历史列表：费用与计量字段齐（formatOrder 读 charged_kwh/total_cents 等）。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/history?page=1&page_size=20",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history 应 200: {body}");
    let items = body["data"]["items"].as_array().unwrap();
    assert!(body["data"]["total"].as_i64().unwrap() >= 1);
    let mine = items
        .iter()
        .find(|i| i["order_no"] == order_no.as_str())
        .expect("历史应包含本人订单");
    assert_eq!(mine["status"], "completed");
    assert_eq!(mine["charged_kwh"], "1.2345", "电量保持 DECIMAL 文本");
    assert_eq!(mine["charged_seconds"], 600);
    assert_eq!(mine["electric_cents"], 50);
    assert_eq!(mine["service_cents"], 30);
    assert_eq!(mine["total_cents"], 80);
    assert!(mine["started_at"].is_string());
    assert!(mine["ended_at"].is_string());

    // 状态过滤：charging 视角看不到已完订单。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/history?status=charging",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["order_no"] == order_no.as_str())
            .count(),
        0,
        "charging 过滤不应包含已完订单"
    );
    // 非法状态 → 400。
    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/history?status=bogus",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 非法分页 → 400。
    let (status, _) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/history?page_size=999",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 详情：冻结展示、计费状态、截止、支付摘要齐。
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/{order_no}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail 应 200: {body}");
    let d = &body["data"];
    assert_eq!(d["order_no"], order_no.as_str());
    assert_eq!(d["status"], "completed");
    assert_eq!(d["device_id"], device_id.as_str());
    assert_eq!(d["port_no"], 1);
    assert_eq!(d["charged_kwh"], "1.2345");
    assert_eq!(d["total_cents"], 80);
    assert_eq!(d["electric_cents"], 50);
    assert_eq!(d["service_cents"], 30);
    assert_eq!(d["shortfall_cents"], 0);
    assert_eq!(d["billing_status"], "done", "计费任务状态原样输出");
    assert_eq!(d["billing_cutoff"]["reason"], "stop_request");
    assert!(!d["billing_cutoff"]["cutoff_at"].is_null());
    assert_eq!(d["card_operations"], json!([]));
    assert_eq!(d["start_confirmation_pending"], false);
    assert_eq!(d["payment"]["order_no"], merchant_no.as_str());
    assert_eq!(d["payment"]["paid_cents"], 100);
    assert_eq!(d["refunds"], json!([]));
    assert!(d["display"].is_object(), "冻结展示应输出");
    assert_eq!(d["display"]["duration"][0]["label"], "功率");
    assert!(d["offer"].is_object(), "冻结套餐应输出");
    assert!(d["tariff"].is_object(), "冻结资费应输出");
    assert_eq!(d["rule_description"], "充值前后规则不变");

    // 越权：他人订单详情 404（与缺失同文案）。
    let other = format!("uq-other-{}", rand_suffix());
    let (other_token, _) = dev_login(&router, &other).await;
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/{order_no}"),
        None,
        Some(&other_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "越权详情应 404: {body}");
    assert_eq!(body["code"], 1004);
    // 未注册单号 → 404。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/no-such-order",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], 1004);
}

/// 场景 2：充电中订单 ongoing 可见；快照无在线会话走回退/待上报语义不 5xx。
#[tokio::test]
async fn ongoing_and_snapshot_fallback() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("uq-on-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;
    let (order_id, order_no, _) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code, 1, rule_id).await;

    // paid 也算进行中：ongoing 可见。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/ongoing",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ongoing 应 200: {body}");
    assert_eq!(body["data"]["order_no"], order_no.as_str());
    assert_eq!(body["data"]["status"], "paid");

    // 推进到 charging。
    mark_charging(&db, &alloc, order_id, &order_no).await;

    // ongoing（order_id 与 order_no 参数皆可）。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/ongoing",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "charging");
    assert_eq!(body["data"]["device_id"], device_id.as_str());
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/ongoing?order_no={order_no}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["order_no"], order_no.as_str());

    // 快照：无遥测无在线会话 → 待上报语义（不 5xx）。
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/ongoing/snapshot?order_id={order_no}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "快照应 200: {body}");
    let s = &body["data"];
    assert_eq!(s["status"], "charging");
    assert_eq!(s["poll_continue"], true);
    assert_eq!(s["next_poll_after_ms"], 5000);
    assert_eq!(s["telemetry_available"], false);
    assert_eq!(s["live_unavailable"], "设备计量暂不可读取");
    assert!(s["elapsed_seconds"].as_i64().unwrap() >= 0);

    // 无进行中订单的用户 → 空状态（非 404）。
    let other = format!("uq-on-empty-{}", rand_suffix());
    let (empty_token, _) = dev_login(&router, &other).await;
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/charge/ongoing",
        None,
        Some(&empty_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({"order_no": "", "status": ""}));

    // 越权快照 → 404。
    let other = format!("uq-on-other-{}", rand_suffix());
    let (other_token, _) = dev_login(&router, &other).await;
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/ongoing/snapshot?order_id={order_no}"),
        None,
        Some(&other_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "越权快照应 404: {body}");
}

/// 场景 3：曲线端点空数据返回空数组；窗口与粒度校验按旧版错误码。
#[tokio::test]
async fn curve_windows_and_validation() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("uq-cv-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;
    let (order_id, order_no, _) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code, 1, rule_id).await;
    mark_charging(&db, &alloc, order_id, &order_no).await;

    // 实时曲线：空数据 → 空数组不报错。
    for window in ["", "last_30min", "last_2h", "last_24h"] {
        let suffix = if window.is_empty() {
            String::new()
        } else {
            format!("&window={window}")
        };
        let (status, body) = req(
            router.clone(),
            "GET",
            &format!("/api/v1/user/charge/ongoing/curve?order_id={order_no}{suffix}"),
            None,
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "curve({window}) 应 200: {body}");
        assert_eq!(body["data"]["series"], json!([]), "空数据应返回空数组");
        assert_eq!(body["data"]["count"], 0);
        assert_eq!(
            body["data"]["window"],
            if window.is_empty() {
                "last_30min"
            } else {
                window
            }
        );
    }
    // 非法窗口 → 400。
    let (status, _) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/ongoing/curve?order_id={order_no}&window=bogus"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 历史曲线：空数据 → 空数组 + 摘要含订单电量文本。
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/{order_no}/curve?granularity=15min"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "历史曲线应 200: {body}");
    assert_eq!(body["data"]["series"], json!([]));
    assert_eq!(body["data"]["granularity"], "15min");
    assert_eq!(body["data"]["boundary_approximate"], true);
    // 非法粒度 → 400。
    let (status, _) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/{order_no}/curve?granularity=bogus"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 未开始订单的历史曲线 → 409 2009（端口 2 避免同秒同端口单号冲突）。
    let port_code2 = seed_extra_port(&db, &device_id).await;
    let (_order_id2, order_no2, _) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code2, 2, rule_id).await;
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/user/charge/{order_no2}/curve"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "未开始应 409: {body}");
    assert_eq!(body["code"], 2009);
}

/// 场景 4：扫码解析与端口查询（已导入设备返回端口集合与运营态）。
#[tokio::test]
async fn scan_resolve_and_port() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (station_id, device_id, port_code, _rule_id) = seed_station_device_rule(&db, &alloc).await;
    // 心跳在线（2 分钟内）→ available 判定成立。
    sqlx::query("UPDATE `device` SET `last_seen_at` = UTC_TIMESTAMP(3) WHERE `device_id` = ?")
        .bind(&device_id)
        .execute(&*db)
        .await
        .unwrap();

    // 设备码解析：kind=device + 端口集合 + 站点与运营态。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/resolve",
        Some(json!({"code": device_id})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "设备码解析应 200: {body}");
    assert_eq!(body["data"]["kind"], "device");
    assert_eq!(body["data"]["device_id"], device_id.as_str());
    assert_eq!(body["data"]["station_id"], station_id);
    assert_eq!(body["data"]["device_status"], "enabled");
    let ports = body["data"]["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0]["port_id"], port_code.as_str());
    assert_eq!(ports[0]["port_no"], 1);
    assert_eq!(ports[0]["port_status"], "idle");
    assert_eq!(ports[0]["online"], true);
    assert_eq!(ports[0]["available"], true);
    assert_eq!(ports[0]["device_status"], "enabled");

    // 端口码解析：kind=port + 单端口对象。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/resolve",
        Some(json!({"code": port_code})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "端口码解析应 200: {body}");
    assert_eq!(body["data"]["kind"], "port");
    assert_eq!(body["data"]["port"]["port_id"], port_code.as_str());
    assert_eq!(body["data"]["port"]["available"], true);

    // HTTPS 链接提取 code 参数。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/resolve",
        Some(json!({"code": format!("https://qr.example.com/p?x=1&code={port_code}")})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "HTTPS 链接解析应 200: {body}");
    assert_eq!(body["data"]["kind"], "port");

    // 非法码 → 400；未注册码 → 404 1004。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/resolve",
        Some(json!({"code": "bad code with spaces"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/resolve",
        Some(json!({"code": format!("dev-none-{}", rand_suffix())})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "未注册设备应 404: {body}");
    assert_eq!(body["code"], 1004);

    // 端口查询：命中返回单端口；设备码按旧版输出 port not found。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/port",
        Some(json!({"port_id": port_code})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "端口查询应 200: {body}");
    assert_eq!(body["data"]["port_id"], port_code.as_str());
    assert_eq!(body["data"]["port_status"], "idle");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/port",
        Some(json!({"port_id": device_id})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "设备码查端口应 404: {body}");
    assert_eq!(body["code"], 1004);
    // 非法端口编码 → 400。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/scan/port",
        Some(json!({"port_id": "!!"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 场景 5：旧形状停止路由（charging 订单停止成功；越权 404；重放幂等）。
#[tokio::test]
async fn stop_body_shape_and_forbidden() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (_station, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("uq-st-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;
    let (order_id, order_no, _) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code, 1, rule_id).await;
    mark_charging(&db, &alloc, order_id, &order_no).await;

    // 旧形状 POST /charge/stop {order_no} → 200 受理。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/charge/stop",
        Some(json!({"order_no": order_no})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "旧形状停止应 200: {body}");
    assert_eq!(body["data"]["order_no"], order_no.as_str());
    assert_eq!(body["data"]["status"], "stopping");
    // 截止冻结与停止命令落库。
    let cutoff: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(cutoff, 1, "停止应冻结计费截止");
    let stop_cmd: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_stop_command` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(stop_cmd, 1, "停止命令应登记一次");

    // 重放：幂等复用同一命令。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/charge/stop",
        Some(json!({"order_no": order_no})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放停止应仍 200");
    let stop_cmd: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_stop_command` WHERE `charge_order_id` = ?",
    )
    .bind(order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(stop_cmd, 1, "重放不得重复建停止命令");

    // 越权：他人订单 → 404。
    let other = format!("uq-st-other-{}", rand_suffix());
    let (other_token, _) = dev_login(&router, &other).await;
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/charge/stop",
        Some(json!({"order_no": order_no})),
        Some(&other_token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "越权停止应 404: {body}");
    assert_eq!(body["code"], 1004);

    // 缺体 → 400。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/charge/stop",
        Some(json!({})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 新形状路由仍可用（回归既有路径；端口 2 避免同秒同端口单号冲突）。
    let port_code3 = seed_extra_port(&db, &device_id).await;
    let (order_id3, order_no3, _) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code3, 2, rule_id).await;
    mark_charging(&db, &alloc, order_id3, &order_no3).await;
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/user/charging/{order_no3}/stop"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "新形状停止应 200: {body}");
}
