//! 阶段 04 批 6a 主缝集成测试：在线卡刷卡首购/续购/幂等重放/拒绝路径，
//! 以及刷卡会话端口占用随结束回执释放的联动。

use chargepilot_mod_charging::{SimulationSink, StartDispatcher};
use chargepilot_mod_payment::scan::TablePortScanner;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use std::sync::Arc;

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// 资金批次互斥锁（共享 dev 库的 flow 测试串行）。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn open_db() -> Option<Arc<sqlx::MySqlPool>> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        eprintln!("跳过：TEST_MYSQL_URL 未设置");
        return None;
    };
    match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => Some(Arc::new(d)),
        Err(e) => {
            eprintln!("跳过：数据库连接失败 {e}");
            None
        }
    }
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 测试方案：duration 套餐（id=2，100 分/60 分钟）+ 刷卡策略（上限 120 分钟）。
fn test_scheme() -> Value {
    json!({
        "name": "刷卡测试方案", "remark": "",
        "amount": {"algorithm": "server_energy", "periods": [
            {"electric_cents": 50, "service_cents": 30, "end_minute": 1440}]},
        "packages": [
            {"id": 1, "name": "1元", "mode": "amount", "price_cents": 100},
            {"id": 2, "name": "60分钟", "mode": "duration", "price_cents": 100, "minutes": 60}
        ],
        "policy": {"free_minutes": 0, "min_electric_cents": 0, "max_minutes": 600, "loss_rate_bp": 0, "channel_bp": 10000},
        "stop": {"stop_when_full": true},
        "card": {"package_id": 2, "max_minutes": 120},
        "display": {"duration": [], "energy": []}
    })
}

/// 种子：站点/厂商/设备/端口/规则 + 用户/钱包(500 分)/在线卡。返回上下文 ID。
#[allow(clippy::too_many_arguments)]
async fn seed(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    balance_cents: i64,
    card_status: &str,
) -> (u64, String, String, u64, u64) {
    let station_id = alloc.next_id().await.unwrap();
    let vendor_id = alloc.next_id().await.unwrap();
    let rule_id = alloc.next_id().await.unwrap();
    let user_id = alloc.next_id().await.unwrap();
    let card_id = alloc.next_id().await.unwrap();
    let device_id = format!("dev-c6-{}", rand_suffix());
    let port_code = format!("port-{}", rand_suffix());
    sqlx::query("INSERT INTO `station` (`id`, `name`, `longitude`, `latitude`, `status`) VALUES (?, '刷卡测试站', 120.1, 30.2, 'active')")
        .bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `vendor` (`id`, `vendor_code`, `vendor_name`, `adapter_class`, `status`) VALUES (?, ?, '刷卡测试厂商', 'dc589', 'enabled')")
        .bind(vendor_id as i64).bind(format!("v{}", rand_suffix())).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device` (`device_id`, `vendor_id`, `station_id`, `status`) VALUES (?, ?, ?, 'enabled')")
        .bind(&device_id).bind(vendor_id as i64).bind(station_id as i64).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `device_port` (`device_id`, `port_no`, `port_code`, `status`) VALUES (?, 1, ?, 'idle')")
        .bind(&device_id).bind(&port_code).execute(db).await.unwrap();
    let spec = json!({"scheme": test_scheme()});
    sqlx::query("INSERT INTO `pricing_rule` (`id`, `name`, `station_id`, `device_id`, `version`, `status`, `spec_json`) VALUES (?, '刷卡测试规则', ?, NULL, 1, 'active', CAST(? AS JSON))")
        .bind(rule_id as i64).bind(station_id as i64).bind(spec.to_string()).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `user` (`id`, `openid`, `nickname`, `status`) VALUES (?, CAST(? AS BINARY), '刷卡测试用户', 'active')")
        .bind(user_id as i64).bind(format!("c6-{user_id}")).execute(db).await.unwrap();
    sqlx::query("INSERT INTO `wallet_account` (`id`, `user_id`, `balance_cents`, `frozen_cents`, `version`, `status`) VALUES (?, ?, ?, 0, 1, 'active')")
        .bind(alloc.next_id().await.unwrap() as i64).bind(user_id as i64).bind(balance_cents)
        .execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO `online_card` (`id`, `card_no`, `user_id`, `status`) VALUES (?, ?, ?, ?)",
    )
    .bind(card_id as i64)
    .bind(format!("66{user_id}"))
    .bind(user_id as i64)
    .bind(card_status)
    .execute(db)
    .await
    .unwrap();
    (station_id, device_id, port_code, user_id, card_id)
}

/// 尾部自清理：按前缀删除本批种子（流水保留）。
async fn cleanup(
    db: &sqlx::MySqlPool,
    device_ids: &[String],
    user_ids: &[u64],
    station_id: u64,
    rule_id: u64,
) {
    for d in device_ids {
        let _ = sqlx::query(
            "DELETE cc FROM `card_charge` cc \
             JOIN `charge_order` c ON c.`id` = cc.`charge_order_id` WHERE c.`device_id` = ?",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE co FROM `card_operation` co \
             JOIN `charge_order` c ON c.`id` = co.`charge_order_id` WHERE c.`device_id` = ?",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE r FROM `refund_record` r \
             JOIN `charge_order` c ON c.`id` = r.`biz_id` WHERE c.`device_id` = ?",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_fee_receipt` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_billing_job` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_end_receipt` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_order_pricing` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_command` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `active_port_charge` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_payment_intent` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query(
            "DELETE FROM `charge_billing_cutoff` WHERE `charge_order_id` IN \
             (SELECT `id` FROM `charge_order` WHERE `device_id` = ?)",
        )
        .bind(d)
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `charge_order` WHERE `device_id` = ?")
            .bind(d)
            .execute(db)
            .await;
        let _ = sqlx::query(
            "DELETE p FROM `payment_order` p \
             LEFT JOIN `charge_order` c ON c.`id` = p.`biz_id` AND p.`biz_type` = 'charge' \
             WHERE p.`order_no` LIKE 'P%' AND c.`id` IS NULL \
               AND p.`id` IN (SELECT `payment_order_id` FROM `charge_payment_intent`)",
        )
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `device_port` WHERE `device_id` = ?")
            .bind(d)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `device` WHERE `device_id` = ?")
            .bind(d)
            .execute(db)
            .await;
    }
    for u in user_ids {
        let _ = sqlx::query("DELETE FROM `online_card` WHERE `user_id` = ?")
            .bind(*u as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `wallet_account` WHERE `user_id` = ?")
            .bind(*u as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(*u as i64)
            .execute(db)
            .await;
    }
    let _ = sqlx::query("DELETE FROM `station` WHERE `id` = ?")
        .bind(station_id as i64)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM `pricing_rule` WHERE `id` = ?")
        .bind(rule_id as i64)
        .execute(db)
        .await;
}

#[tokio::test]
async fn card_swipe_full_flow() {
    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(db) = open_db().await else {
        return;
    };
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 60));
    alloc.warm_up().await.unwrap();
    let (station_id, device_id, port_code, user_id, _card_id) =
        seed(db.as_ref(), &alloc, 500, "active").await;
    let rule_id: u64 = sqlx::query_scalar("SELECT `id` FROM `pricing_rule` WHERE `station_id` = ?")
        .bind(station_id as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    let card_no: String =
        sqlx::query_scalar("SELECT CAST(`card_no` AS CHAR) FROM `online_card` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();

    let service = chargepilot_mod_card::SwipeService {
        pool: db.as_ref().clone(),
        allocator: (*alloc).clone(),
        scanner: Arc::new(TablePortScanner::new(db.as_ref().clone())),
    };
    let now = chrono::Utc::now();

    // 1. 首购：扣款 100 分（500→400），订单 paid + 会话 + confirming 操作 + 出站事件。
    let first = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: now.timestamp_micros(),
    };
    let out = service.apply(&first).await.expect("首购应成功");
    assert_eq!(out.kind, "start");
    assert_eq!(out.status, "confirming");
    assert_eq!(out.price_cents, 100);
    assert_eq!(out.minutes, 60);
    assert!(!out.replayed);
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance, 400, "首购应扣款 100 分");
    let (order_status, pay_id): (String, u64) = sqlx::query_as(
        "SELECT c.`status`, c.`payment_order_id` FROM `charge_order` c WHERE c.`id` = ?",
    )
    .bind(out.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(order_status, "paid");
    let (pay_total, pay_method): (i64, String) =
        sqlx::query_as("SELECT `paid_cents`, `pay_method` FROM `payment_order` WHERE `id` = ?")
            .bind(pay_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(
        (pay_total, pay_method),
        (100, "balance".to_string()),
        "刷卡支付单应为已付余额单"
    );
    let (session_paid, session_minutes, active_port): (i64, u16, Option<String>) = sqlx::query_as(
        "SELECT `paid_cents`, `purchased_minutes`, `active_port` FROM `card_charge` WHERE `charge_order_id` = ?",
    )
    .bind(out.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!((session_paid, session_minutes), (100, 60));
    assert_eq!(active_port.as_deref(), Some(port_code.as_str()));
    let outbound: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM `domain_event` WHERE `kind` = 'charge_payment_confirmed' \
         AND CAST(`payload_json` AS CHAR) LIKE ?",
    )
    .bind(format!("%{}%", out.charge_order_id))
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(outbound, 1, "首购应出站启动驱动事件");

    // 2. 启动闭环：出站事件投递 → 命令下发（simulation 直接 ack）→ 订单 charging。
    let dispatcher = StartDispatcher {
        pool: db.as_ref().clone(),
        allocator: (*alloc).clone(),
        sink: Arc::new(SimulationSink),
    };
    dispatcher
        .handle(&json!({
            "event_id": format!("E{}", rand_suffix()),
            "charge_order_id": out.charge_order_id,
            "order_no": sqlx::query_scalar::<_, String>(
                "SELECT `order_no` FROM `charge_order` WHERE `id` = ?",
            )
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap(),
        }))
        .await
        .expect("启动投递应成功");
    let order_status: String =
        sqlx::query_scalar("SELECT `status` FROM `charge_order` WHERE `id` = ?")
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(order_status, "charging", "启动回执后订单应 charging");

    // 3. 续购：同卡同端口新事件，累计 120 分钟（上限恰好满足）。
    let extend = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    let out2 = service.apply(&extend).await.expect("续购应成功");
    assert_eq!(out2.kind, "extend");
    assert_eq!(out2.status, "confirmed");
    let (session_paid, session_minutes): (i64, u16) = sqlx::query_as(
        "SELECT `paid_cents`, `purchased_minutes` FROM `card_charge` WHERE `charge_order_id` = ?",
    )
    .bind(out.charge_order_id as i64)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(
        (session_paid, session_minutes),
        (200, 120),
        "续购后累计 200 分/120 分钟"
    );
    let pay_paid: i64 =
        sqlx::query_scalar("SELECT `paid_cents` FROM `payment_order` WHERE `id` = ?")
            .bind(pay_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(pay_paid, 200, "续购应累计支付单实付");
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance, 300, "续购再扣 100 分");

    // 4. 同事件重放：replayed=true，不再扣款。
    let replay = service.apply(&extend).await.expect("重放应成功");
    assert!(replay.replayed);
    assert_eq!(replay.operation_id, out2.operation_id);
    let balance: i64 =
        sqlx::query_scalar("SELECT `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(user_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(balance, 300, "重放不得重复扣款");

    // 5. 累计上限：120 分钟已满，再续购拒绝（上限优先于余额语义）。
    let over = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    let err = service.apply(&over).await.expect_err("超上限应拒绝");
    assert_eq!(err, chargepilot_mod_card::SwipeError::CardDeclined);
    assert!(err.is_declined(), "拒绝应属 2009 族");

    // 6. 余额不足：余额降到 50 分后重置累计再刷 → CardBalance。
    sqlx::query("UPDATE `wallet_account` SET `balance_cents` = 50 WHERE `user_id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    sqlx::query("UPDATE `card_charge` SET `purchased_minutes` = 60 WHERE `charge_order_id` = ?")
        .bind(out.charge_order_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let poor = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    let err = service.apply(&poor).await.expect_err("余额不足应拒绝");
    assert_eq!(err, chargepilot_mod_card::SwipeError::CardBalance);

    // 7. 时效：事件时刻 31 秒前拒绝。
    let late = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: (chrono::Utc::now() - chrono::Duration::seconds(31))
            .timestamp_micros(),
    };
    let err = service.apply(&late).await.expect_err("超时应拒绝");
    assert_eq!(err, chargepilot_mod_card::SwipeError::Expired);

    // 8. 卡挂失后拒绝。
    sqlx::query("UPDATE `wallet_account` SET `balance_cents` = 500 WHERE `user_id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    sqlx::query("UPDATE `online_card` SET `status` = 'lost' WHERE `user_id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let lost = chargepilot_mod_card::SwipeInput {
        card_no: card_no.clone(),
        event_id: format!("EVT-{}", rand_suffix()),
        port_id: port_code.clone(),
        occurred_at_unix_micros: chrono::Utc::now().timestamp_micros(),
    };
    let err = service.apply(&lost).await.expect_err("挂失卡应拒绝");
    assert_eq!(err, chargepilot_mod_card::SwipeError::CardDeclined);

    // 9. 结束回执联动：刷卡会话端口占用释放。
    sqlx::query("UPDATE `online_card` SET `status` = 'active' WHERE `user_id` = ?")
        .bind(user_id as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    let started_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT `started_at` FROM `charge_order` WHERE `id` = ?")
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    let stop_command_id = format!("stop-{}", rand_suffix());
    chargepilot_mod_charging::apply_end_receipt(
        db.as_ref(),
        &chargepilot_mod_charging::EndReceiptInput {
            charge_order_id: out.charge_order_id,
            order_no: sqlx::query_scalar::<_, String>(
                "SELECT `order_no` FROM `charge_order` WHERE `id` = ?",
            )
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap(),
            start_command_id: sqlx::query_scalar::<_, String>(
                "SELECT CAST(`command_id` AS CHAR) FROM `charge_command` WHERE `charge_order_id` = ?",
            )
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap(),
            stop_command_id: stop_command_id.clone(),
            device_id: device_id.clone(),
            port_no: 1,
            meter: chargepilot_domain::charging_actual::EndMeter {
                segments: vec![chargepilot_domain::charging_actual::MeterSegment {
                    started_at,
                    ended_at: started_at + chrono::Duration::minutes(30),
                    energy_wh: 0,
                    peak_w: 0,
                    power_w: None,
                }],
                charged_wh: 0,
                charged_seconds: 1800,
                ended_at: started_at + chrono::Duration::minutes(30),
                stop_reason: 0,
            },
        },
    )
    .await
    .expect("结束回执应成功");
    let active_port: Option<String> =
        sqlx::query_scalar("SELECT `active_port` FROM `card_charge` WHERE `charge_order_id` = ?")
            .bind(out.charge_order_id as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(active_port, None, "结束回执应释放刷卡会话端口占用");

    cleanup(db.as_ref(), &[device_id], &[user_id], station_id, rule_id).await;
}

/// domain 纯规则烟测：卡号校验与续购判定（详测在 domain 单测）。
#[test]
fn card_rules_smoke() {
    assert!(chargepilot_domain::card::valid_card_no("12345678"));
    assert!(!chargepilot_domain::card::valid_card_no("0123"));
    let p = chargepilot_domain::charging_scheme::Package {
        id: 2,
        name: "60分钟".into(),
        mode: "duration".into(),
        price_cents: 100,
        minutes: 60,
        kwh: 0,
    };
    assert!(chargepilot_domain::card::can_extend(
        1, 1, &p, 60, 120, 100, false
    ));
    assert!(!chargepilot_domain::card::can_extend(
        1, 1, &p, 60, 120, 99, false
    ));
}
