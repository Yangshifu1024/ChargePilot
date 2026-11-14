//! // 票 04 将把进程内互斥锁统一为跨进程端口锁；此前豁免持锁跨 await 检查。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 阶段 04 批 4 主缝集成测试：充电状态机事件驱动全链——支付确认事件 →
//! 启动命令与端口预占 → 设备回执 charging → 用户停止（计费截止冻结）→
//! 结束回执（订单完成 + 计费任务 + 出站事件）。

mod support;

use axum::http::StatusCode;
use chargepilot_mod_charging::{SimulationSink, StartDispatcher};
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::json;
use std::sync::Arc;
use support::{TestApp, rand_suffix, req};

/// 状态机测试互斥锁：端口预占依赖库中唯一事实。
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

/// 开发登录并返回 (jwt, user_id)。
async fn dev_login(router: &axum::Router, openid: &str) -> (String, u64) {
    let (status, body) = req(
        router,
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

/// 种子：站点 + 设备 + 端口 + 规则（与 payment_flow 一致）。
async fn seed_station_device_rule(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
) -> (u64, String, String, u64) {
    let station_id = alloc.next_id().await.unwrap();
    let vendor_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-c4-{}", rand_suffix());
    let port_code = format!("port-{}", rand_suffix());
    let rule_id = alloc.next_id().await.unwrap();
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '状态机测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `status`) VALUES (?, ?, '状态机测试厂商', 'dc589', 'enabled')")
        .bind(vendor_id as i64).bind(format!("v{}", rand_suffix())).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `station_id`, `status`) VALUES (?, ?, ?, 'enabled')")
        .bind(&device_id).bind(vendor_id as i64).bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 1, ?, 'idle')")
        .bind(&device_id).bind(&port_code).execute(db).await.unwrap();
    let scheme = json!({
        "name": "状态机测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [{"id": 1, "name": "1元", "mode": "amount", "price_cents": 100}],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 1, "max_minutes": 600},
        "display": {"duration": [], "energy": []}
    });
    let spec = json!({"scheme": scheme});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`) VALUES (?, '状态机测试规则', ?, NULL, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    (station_id, device_id, port_code, rule_id)
}

/// 创建已支付充电订单（直接复用 apply_payment：模拟确认 → paid 订单 + 出站事件）。
async fn create_paid_order(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    user_id: u64,
    device_id: &str,
    port_code: &str,
    rule_id: u64,
) -> (u64, String, String) {
    // 意图：直接经 checkout 的冻结语义太重，SQL 直插意图与支付单（仅本测试种子）。
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
    // 规则快照（最小可用；真实链路由 checkout 冻结）。
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
         VALUES (?, ?, ?, ?, ?, CAST(? AS BINARY), ?, 1, ?, 0, ?, 1, CAST(? AS JSON), '0.000', \
         600, 0, 0, 100, 4, 600, 'initiated', ?, ?)",
    )
    .bind(&intent_id)
    .bind(&order_no_hint)
    .bind(&merchant_order_no)
    .bind(payment_id as i64)
    .bind(user_id as i64)
    .bind(format!("sim-{}", user_id))
    .bind(device_id)
    .bind(port_code)
    .bind(rule_id as i64)
    .bind(serde_json::to_string(&snapshot).unwrap())
    .bind(expires)
    .bind((rule_id * 100 + 1) as i64)
    .execute(db)
    .await
    .unwrap();
    // 模拟渠道预支付标记（simulation 确认校验用）。
    sqlx::query("INSERT INTO `charge_prepay` (`payment_order_id`, `params_json`, `prepay_id`) VALUES (?, CAST(? AS JSON), ?)")
        .bind(payment_id as i64)
        .bind(json!({"mode": "simulation"}).to_string())
        .bind(format!("SIM{merchant_order_no}"))
        .execute(db)
        .await
        .unwrap();
    // 走真实支付成功事务（批 3 入口）：创建 paid 订单 + 快照冻结 + 出站事件。
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
    assert!(!result.replayed);
    (
        result.charge_order_id,
        result.charge_order_no,
        merchant_order_no,
    )
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn state_machine_full_flow() {
    let app = TestApp::builder()
        .with_wallet()
        .with_simulation_route()
        .with_charging_stop()
        .build()
        .await;
    let router = app.router;
    let db = app.db;
    let alloc = app.allocator;
    let (station_id, device_id, port_code, rule_id) = seed_station_device_rule(&db, &alloc).await;
    let openid = format!("c4-{}", rand_suffix());
    let (token, user_id) = dev_login(&router, &openid).await;

    // 0. 支付成功（批 3 入口）→ paid 订单。
    let (charge_order_id, order_no, _merchant) =
        create_paid_order(&db, &alloc, user_id, &device_id, &port_code, rule_id).await;
    let status: String = sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
        .bind(charge_order_id as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert_eq!(status, "paid");

    // 1. 事件投递：启动命令 + 端口预占 + simulation 闭环回执 → charging。
    let dispatcher = StartDispatcher {
        pool: db.as_ref().clone(),
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
    let (cmd_status, order_status): (String, String) = sqlx::query_as(
        "SELECT cmd.`status`, c.`status` FROM `charge_command` cmd \
         JOIN `charge_order` c ON c.`id` = cmd.`charge_order_id` WHERE cmd.`charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(cmd_status, "acked", "simulation 闭环应直接 ack");
    assert_eq!(order_status, "charging", "启动回执后订单应 charging");
    let occupied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `active_port_charge` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(occupied, 1, "端口应被预占");

    // 2. 事件重放：幂等（不重复建命令）。
    dispatcher.handle(&payload).await.expect("重放投递应成功");
    let cmd_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_command` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(cmd_count, 1, "重放不得重复建命令");

    // 3. 用户停止：截止冻结 + 停止命令。
    let (status, body) = req(
        &router,
        "POST",
        &format!("/api/v1/user/charging/{order_no}/stop"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "用户停止应成功: {body}");
    let cutoff: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(cutoff, 1, "计费截止必须已冻结");
    let stop_cmd: String = sqlx::query_scalar(
        "SELECT `status` FROM `charge_stop_command` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(stop_cmd, "sent");

    // 4. 停止重放：截止点不延长（no-op upsert）。
    let cutoff_at_before: chrono::NaiveDateTime = sqlx::query_scalar(
        "SELECT `cutoff_at` FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    let (status, _) = req(
        &router,
        "POST",
        &format!("/api/v1/user/charging/{order_no}/stop"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停止重放应静默成功");
    let cutoff_at_after: chrono::NaiveDateTime = sqlx::query_scalar(
        "SELECT `cutoff_at` FROM `charge_billing_cutoff` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(cutoff_at_before, cutoff_at_after, "截止点不得延长");

    // 5. 结束回执：订单完成 + 端口释放 + 计费任务 + 出站事件。
    let stop_command_id: String = sqlx::query_scalar(
        "SELECT CAST(`command_id` AS CHAR) FROM `charge_stop_command` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    let start_command_id: String = sqlx::query_scalar(
        "SELECT CAST(`command_id` AS CHAR) FROM `charge_start_receipt` WHERE `charge_order_id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    let now = chrono::Utc::now();
    let meter = chargepilot_domain::charging_actual::EndMeter {
        segments: vec![chargepilot_domain::charging_actual::MeterSegment {
            started_at: now - chrono::Duration::minutes(10),
            ended_at: now,
            energy_wh: 100,
            peak_w: 1500,
            power_w: None,
        }],
        charged_wh: 100,
        charged_seconds: 600,
        ended_at: now,
        stop_reason: 0,
    };
    let replayed = chargepilot_mod_charging::apply_end_receipt(
        db.as_ref(),
        &chargepilot_mod_charging::EndReceiptInput {
            charge_order_id,
            order_no: order_no.clone(),
            start_command_id: start_command_id.clone(),
            stop_command_id: stop_command_id.clone(),
            device_id: device_id.clone(),
            port_no: 1,
            meter,
        },
    )
    .await
    .expect("结束回执应成功");
    assert!(!replayed);
    let (final_status, kwh): (String, String) = sqlx::query_as(
        "SELECT `status`, CAST(`charged_kwh` AS CHAR) FROM `charge_order` WHERE `id` = ?",
    )
    .bind(charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(final_status, "completed");
    assert_eq!(kwh, "0.1000", "Wh→kWh 四位小数");
    let job: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_billing_job` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(job, "pending", "计费任务应已登记");
    let released: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `active_port_charge` WHERE `charge_order_id` = ?")
            .bind(charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(released, 0, "端口占用应已释放");
    let ended_event: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `domain_event` WHERE `kind` = 'charge_ended' AND `payload_json` LIKE ?",
    )
    .bind(format!("%{order_no}%"))
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(ended_event, 1, "结束出站事件应已写入");

    // 6. 结束回执重放：同内容静默确认。
    let meter2 = chargepilot_domain::charging_actual::EndMeter {
        segments: vec![chargepilot_domain::charging_actual::MeterSegment {
            started_at: now - chrono::Duration::minutes(10),
            ended_at: now,
            energy_wh: 100,
            peak_w: 1500,
            power_w: None,
        }],
        charged_wh: 100,
        charged_seconds: 600,
        ended_at: now,
        stop_reason: 0,
    };
    let replayed2 = chargepilot_mod_charging::apply_end_receipt(
        db.as_ref(),
        &chargepilot_mod_charging::EndReceiptInput {
            charge_order_id,
            order_no: order_no.clone(),
            start_command_id: start_command_id.clone(),
            stop_command_id,
            device_id,
            port_no: 1,
            meter: meter2,
        },
    )
    .await
    .expect("重放回执应成功");
    assert!(replayed2, "同内容重放应静默确认");

    // 清理维表（流水保留）。
    let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
        .bind(format!("dev-c4-{}", rand_suffix()))
        .execute(db.as_ref())
        .await;
    let _ = station_id;
}
