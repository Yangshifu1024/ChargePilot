//! 集成测试在持锁场景下跨 await；已配置 clippy 豁免。
#![allow(clippy::await_holding_lock)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::wildcard_enum_match_arm
)]

//! 主缝集成测试：C 端手机号绑定/解绑、在线卡列表/状态变更、
//! 订单反馈提交（真 MySQL + 真 Redis）。
//! 断言只看 HTTP 响应（信封、码值、形状与文案），不看内部实现；
//! development 模式明文绑定与 wechat 模式准入语义与对外 API 契约一致。

mod support;

use axum::http::StatusCode;
use chargepilot_platform::ids::SegmentAllocator;
use serde_json::{Value, json};
use std::sync::Arc;
use support::{TestApp, rand_suffix};

/// 共享 dev 库的 flow 测试串行（资金与唯一键边界互斥）：请求委托共享
/// req_str（登录频次按每请求随机 IP 计数，与限流断言无交集）。
async fn post_json(
    router: &axum::Router,
    path: &str,
    body: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    support::req_str(router, "POST", path, Some(body), bearer).await
}

async fn get_with_bearer(router: &axum::Router, path: &str, token: &str) -> (StatusCode, Value) {
    support::req_str(router, "GET", path, None, Some(token)).await
}

/// development 登录辅助：返回 (jwt, user_id 十进制字符串)。
async fn dev_login(router: &axum::Router, account: &str) -> (String, String) {
    let (status, body) = post_json(
        router,
        "/api/v1/user/auth/login",
        &format!(r#"{{"code":"dev:{account}"}}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "开发登录应成功: {body}");
    (
        body["data"]["jwt"].as_str().unwrap().to_string(),
        body["data"]["user_id"].as_str().unwrap().to_string(),
    )
}

/// 直插在线卡（card_no 按 id 唯一构造），返回卡号。
async fn seed_card(db: &sqlx::MySqlPool, alloc: &SegmentAllocator, user_id: u64) -> String {
    let card_id = alloc.next_id().await.unwrap();
    let card_no = format!("9{card_id}");
    sqlx::query("INSERT INTO `online_card` (`id`, `card_no`, `user_id`, `status`) VALUES (?, ?, ?, 'active')")
        .bind(card_id as i64)
        .bind(&card_no)
        .bind(user_id as i64)
        .execute(db)
        .await
        .unwrap();
    card_no
}

/// 直插充电订单（默认 completed），返回 (订单 ID, 业务单号)。
async fn seed_order(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    user_id: u64,
    status: &str,
) -> (u64, String) {
    let order_id = alloc.next_id().await.unwrap();
    let order_no = format!("C{order_id}");
    let month = chargepilot_mod_order::month_start_of(chrono::Utc::now());
    sqlx::query(
        "INSERT INTO `charge_order` (`id`, `order_no`, `user_id`, `device_id`, `port_no`, \
         `port_code`, `status`, `created_month`) VALUES (?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(order_id as i64)
    .bind(&order_no)
    .bind(user_id as i64)
    .bind(format!("dev-fb-{}", rand_suffix()))
    .bind(format!("pc-{order_id}"))
    .bind(status)
    .bind(month)
    .execute(db)
    .await
    .unwrap();
    (order_id, order_no)
}

/// 尾部自清理：按用户与订单 ID 删除本批种子。
async fn cleanup(db: &sqlx::MySqlPool, user_ids: &[u64], order_ids: &[u64]) {
    for oid in order_ids {
        let _ = sqlx::query("DELETE FROM `feedback` WHERE `order_id` = ?")
            .bind(*oid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `charge_order` WHERE `id` = ?")
            .bind(*oid as i64)
            .execute(db)
            .await;
    }
    for uid in user_ids {
        let _ = sqlx::query(
            "DELETE a FROM `online_card_audit` a \
             JOIN `online_card` c ON c.`id` = a.`card_id` WHERE c.`user_id` = ?",
        )
        .bind(*uid as i64)
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM `online_card` WHERE `user_id` = ?")
            .bind(*uid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `wallet_account` WHERE `user_id` = ?")
            .bind(*uid as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `user` WHERE `id` = ?")
            .bind(*uid as i64)
            .execute(db)
            .await;
    }
}

async fn open_db() -> Option<Arc<sqlx::MySqlPool>> {
    let db_url = support::mysql_url();
    match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => Some(Arc::new(d)),
        Err(e) => {
            panic!("数据库连接失败：{e}");
        }
    }
}

/// 场景 1：development 登录 → 明文绑定 → profile 可见 → 解绑 → profile 消失；
/// 附无效号码与 wechat 模式准入语义。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn phone_bind_unbind_flow() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let router = TestApp::builder().build().await.router;
    let db = open_db().await.unwrap();
    let account = format!("it-ph-{}", rand_suffix());
    let (jwt, user_id) = dev_login(&router, &account).await;
    let uid: u64 = user_id.parse().unwrap();
    // 手机号按用户尾号唯一构造，避开全局 uk_phone 冲突。
    let phone = format!("138{:08}", uid % 100_000_000);

    // 1. 绑定：200，bound=true，掩码保留前 3 后 4。
    let (status, body) = post_json(
        &router,
        "/api/v1/user/phone/bind",
        &json!({"phone": phone}).to_string(),
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "绑定应成功: {body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["bound"], true);
    let masked = phone.as_str();
    assert_eq!(
        body["data"]["phone_masked"],
        format!("{}****{}", &masked[..3], &masked[7..]),
        "掩码保留前 3 后 4"
    );
    let stored: Option<String> = sqlx::query_scalar("SELECT `phone` FROM `user` WHERE `id` = ?")
        .bind(uid as i64)
        .fetch_one(&*db)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(phone.as_str()), "明文入库");

    // 2. profile 可见。
    let (status, body) = get_with_bearer(&router, "/api/v1/user/profile", &jwt).await;
    assert_eq!(status, StatusCode::OK, "profile 应成功: {body}");
    assert_eq!(body["data"]["phone_bound"], true);

    // 3. 无效号码：400/1005 固定文案。
    let (status, body) = post_json(
        &router,
        "/api/v1/user/phone/bind",
        r#"{"phone":"12345"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 1005);
    assert_eq!(body["message"], "请输入有效的中国大陆手机号");

    // 4. 解绑：200，bound=false + unbound=true；phone 置 NULL。
    let (status, body) = post_json(&router, "/api/v1/user/phone/unbind", "null", Some(&jwt)).await;
    assert_eq!(status, StatusCode::OK, "解绑应成功: {body}");
    assert_eq!(body["data"]["bound"], false);
    assert_eq!(body["data"]["unbound"], true);
    let stored: Option<String> = sqlx::query_scalar("SELECT `phone` FROM `user` WHERE `id` = ?")
        .bind(uid as i64)
        .fetch_one(&*db)
        .await
        .unwrap();
    assert_eq!(stored, None, "解绑应置 NULL");
    let (status, body) = get_with_bearer(&router, "/api/v1/user/profile", &jwt).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["phone_bound"], false);

    // 5. 重复解绑幂等成功。
    let (status, _) = post_json(&router, "/api/v1/user/phone/unbind", "null", Some(&jwt)).await;
    assert_eq!(status, StatusCode::OK);

    cleanup(&db, &[uid], &[]).await;
}

/// 场景 1b：wechat 模式准入——明文与 code 互斥；交换能力未接入按暂不可用拒绝。
/// 登录走 development app（wechat 交换未实现无法建会话），鉴权链路（JWT +
/// Redis 会话）与登录模式无关，同一凭据在 wechat app 上同样有效。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn phone_bind_wechat_mode_admission() {
    let dev_router = TestApp::builder().build().await.router;
    let router = TestApp::builder()
        .with_exchanger(chargepilot_mod_identity::wechat::Exchanger::Wechat {
            appid: "wx-test".into(),
            secret: "test".into(),
        })
        .build()
        .await
        .router;
    let account = format!("it-wx-{}", rand_suffix());
    let (jwt, _) = dev_login(&dev_router, &account).await;

    // 仅明文：正式路径不得提交明文。
    let (status, body) = post_json(
        &router,
        "/api/v1/user/phone/bind",
        r#"{"phone":"13812345678"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 1005);
    assert_eq!(body["message"], "请使用微信手机号授权凭证");

    // 明文 + code 同带：同上互斥拒绝。
    let (status, body) = post_json(
        &router,
        "/api/v1/user/phone/bind",
        r#"{"phone":"13812345678","code":"auth-code"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "请使用微信手机号授权凭证");

    // 仅 code：交换能力未接入，统一暂不可用。
    let (status, body) = post_json(
        &router,
        "/api/v1/user/phone/bind",
        r#"{"code":"auth-code"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], 5003);
    assert_eq!(body["message"], "手机号授权暂不可用");
}

/// 场景 2：直插在线卡 → 列表可见、字段与对外 API 契约一致（id 数字、user_id 字符串）。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn cards_list_flow() {
    let router = TestApp::builder().build().await.router;
    let db = open_db().await.unwrap();
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 70));
    alloc.warm_up().await.unwrap();
    let account = format!("it-cards-{}", rand_suffix());
    let (jwt, user_id) = dev_login(&router, &account).await;
    let uid: u64 = user_id.parse().unwrap();
    let card_no = seed_card(&db, &alloc, uid).await;

    // 列表：items 单元素，字段形状与对外 API 契约 JSON 一致。
    let (status, body) = get_with_bearer(&router, "/api/v1/user/cards", &jwt).await;
    assert_eq!(status, StatusCode::OK, "列表应成功: {body}");
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "应只含本人未解绑卡");
    let row = &items[0];
    assert!(row["id"].is_u64(), "id 为数字");
    assert_eq!(row["card_no"], card_no.as_str());
    assert_eq!(row["user_id"], user_id.as_str(), "user_id 为十进制字符串");
    assert_eq!(row["status"], "active");

    cleanup(&db, &[uid], &[]).await;
}

/// 场景 3：状态操作——挂失成功带审计；非法状态 400；他人卡 404；
/// 解绑成功后列表消失且再操作 404。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn card_status_change_flow() {
    let router = TestApp::builder().build().await.router;
    let db = open_db().await.unwrap();
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 70));
    alloc.warm_up().await.unwrap();
    let owner = format!("it-cst-{}", rand_suffix());
    let other = format!("it-cst-o-{}", rand_suffix());
    let (jwt, user_id) = dev_login(&router, &owner).await;
    let uid: u64 = user_id.parse().unwrap();
    let (_other_jwt, other_id) = dev_login(&router, &other).await;
    let other_uid: u64 = other_id.parse().unwrap();
    let card_no = seed_card(&db, &alloc, uid).await;
    let other_card = seed_card(&db, &alloc, other_uid).await;

    // 1. 非法状态（active 属运营可写）：400/1005 固定文案。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{card_no}/status"),
        r#"{"status":"active"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "启用仅运营可写: {body}");
    assert_eq!(body["code"], 1005);
    assert_eq!(body["message"], "仅可挂失或解绑自己的卡");

    // 2. disabled 同样拒绝。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{card_no}/status"),
        r#"{"status":"disabled"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "仅可挂失或解绑自己的卡");

    // 3. 他人卡：404 不泄露存在性。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{other_card}/status"),
        r#"{"status":"lost"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "他人卡应 404: {body}");
    assert_eq!(body["code"], 1004);
    assert_eq!(body["message"], "卡不存在");

    // 4. 挂失成功：响应回显 status；落库 + user_lost 审计。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{card_no}/status"),
        r#"{"status":"lost"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "挂失应成功: {body}");
    assert_eq!(body["data"]["status"], "lost");
    let (stored, action): (String, Option<String>) = sqlx::query_as(
        "SELECT CAST(c.`status` AS CHAR), \
                (SELECT a.`action` FROM `online_card_audit` a \
                 JOIN `online_card` c2 ON c2.`id` = a.`card_id` \
                 WHERE c2.`user_id` = ? AND a.`action` = 'user_lost' LIMIT 1) \
         FROM `online_card` c WHERE c.`card_no` = ?",
    )
    .bind(uid as i64)
    .bind(&card_no)
    .fetch_one(&*db)
    .await
    .unwrap();
    assert_eq!(stored, "lost");
    assert_eq!(action.as_deref(), Some("user_lost"), "应写用户挂失审计");

    // 5. 解绑成功：列表消失。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{card_no}/status"),
        r#"{"status":"unbound"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "解绑应成功: {body}");
    assert_eq!(body["data"]["status"], "unbound");
    let (status, body) = get_with_bearer(&router, "/api/v1/user/cards", &jwt).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["data"]["items"].as_array().unwrap();
    assert!(
        items.iter().all(|i| i["card_no"] != card_no.as_str()),
        "解绑后不得再出现在列表"
    );

    // 6. 解绑后再操作：锁读排除已解绑，统一 404。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/cards/{card_no}/status"),
        r#"{"status":"lost"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "解绑后不可再变更: {body}");
    assert_eq!(body["message"], "卡不存在");

    cleanup(&db, &[uid, other_uid], &[]).await;
}

/// 场景 4：反馈提交——成功、重复 409、他人订单 404、非完成状态 404、
/// 业务单号提交、校验失败 400。
#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn feedback_submit_flow() {
    let router = TestApp::builder().build().await.router;
    let db = open_db().await.unwrap();
    let alloc = Arc::new(SegmentAllocator::new((*db).clone(), 70));
    alloc.warm_up().await.unwrap();
    let account = format!("it-fb-{}", rand_suffix());
    let stranger = format!("it-fb-s-{}", rand_suffix());
    let (jwt, user_id) = dev_login(&router, &account).await;
    let uid: u64 = user_id.parse().unwrap();
    let (stranger_jwt, stranger_id) = dev_login(&router, &stranger).await;
    let stranger_uid: u64 = stranger_id.parse().unwrap();
    let (order_id, order_no) = seed_order(&db, &alloc, uid, "completed").await;
    let (paid_order_id, _) = seed_order(&db, &alloc, uid, "paid").await;

    // 1. 校验失败：评分越界 400/1005。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        r#"{"rating":6,"category":"rating"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "评分越界应 400: {body}");
    assert_eq!(body["code"], 1005);
    assert_eq!(body["message"], "评分或反馈类型无效");

    // 2. 非法分类同拒绝。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        r#"{"rating":3,"category":"other"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "评分或反馈类型无效");

    // 3. 非 HTTPS 图片拒绝。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        r#"{"rating":3,"category":"complaint","images":["http://x.com/a.jpg"]}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "图片链接必须为 HTTPS");

    // 4. 他人订单（数字主键，归属在提交事务核对）：404 固定文案。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        r#"{"rating":3,"category":"rating"}"#,
        Some(&stranger_jwt),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "他人订单应 404: {body}");
    assert_eq!(body["code"], 1004);
    assert_eq!(body["message"], "订单不存在或不可评价");

    // 5. 非完成状态（paid）：404。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{paid_order_id}/feedback"),
        r#"{"rating":3,"category":"rating"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "未完成订单不可评价: {body}");
    assert_eq!(body["message"], "订单不存在或不可评价");

    // 6. 提交成功（数字主键）：submitted=true，feedback_id 数字。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        &json!({
            "rating": 4,
            "category": "suggestion",
            "content": "希望增加快充端口",
            "images": ["https://img.example.com/a.jpg"]
        })
        .to_string(),
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "提交应成功: {body}");
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["submitted"], true);
    assert!(body["data"]["feedback_id"].is_u64(), "feedback_id 为数字");
    let feedback_id = body["data"]["feedback_id"].as_u64().unwrap();
    let row: (u8, String, Option<String>, String, Option<String>) = sqlx::query_as(
        "SELECT `rating`, CAST(`category` AS CHAR), CAST(`content` AS CHAR), \
                CAST(`status` AS CHAR), CAST(`images_json` AS CHAR) \
         FROM `feedback` WHERE `id` = ?",
    )
    .bind(feedback_id as i64)
    .fetch_one(&*db)
    .await
    .unwrap();
    assert_eq!(row.0, 4);
    assert_eq!(row.1, "suggestion");
    assert_eq!(row.2.as_deref(), Some("希望增加快充端口"));
    assert_eq!(row.3, "pending", "初始状态 pending");
    assert!(row.4.is_some(), "images 序列化后入库");

    // 7. 重复提交：409/2009 固定文案。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_id}/feedback"),
        r#"{"rating":5,"category":"rating"}"#,
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "重复提交应 409: {body}");
    assert_eq!(body["code"], 2009);
    assert_eq!(body["message"], "该订单已评价");

    // 8. 按业务单号提交另一笔订单：解析 + 归属一并核对。
    let (order_id2, order_no2) = seed_order(&db, &alloc, uid, "refunded").await;
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_no2}/feedback"),
        &json!({"rating": 2, "category": "complaint"}).to_string(),
        Some(&jwt),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "业务单号提交应成功: {body}");
    assert_eq!(body["data"]["submitted"], true);
    // 缺省 images 保存 JSON null。
    let images: Option<String> = sqlx::query_scalar(
        "SELECT CAST(`images_json` AS CHAR) FROM `feedback` WHERE `order_id` = ?",
    )
    .bind(order_id2 as i64)
    .fetch_one(&*db)
    .await
    .unwrap();
    assert_eq!(
        images.as_deref(),
        Some("null"),
        "缺省 images 保存 JSON null"
    );
    assert_ne!(order_no, order_no2);

    // 9. 他人业务单号：404 订单不存在（解析阶段核对本人）。
    let (status, body) = post_json(
        &router,
        &format!("/api/v1/user/charge/{order_no2}/feedback"),
        r#"{"rating":3,"category":"rating"}"#,
        Some(&stranger_jwt),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "他人业务单号应 404: {body}");
    assert_eq!(body["message"], "订单不存在");

    cleanup(
        &db,
        &[uid, stranger_uid],
        &[order_id, paid_order_id, order_id2],
    )
    .await;
}
