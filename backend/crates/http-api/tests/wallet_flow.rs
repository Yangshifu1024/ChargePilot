//! 阶段 04 批 2 主缝集成测试：钱包退款申请（预占拆单）、频次冻结、风控审核/解冻全链。
//! 数据准备用 SQL 直插 + 服务级入账函数（credit_recharge_in_tx，真库事务）；
//! 断言 HTTP 响应与库中资金行（spec 测试决策）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::{ApiDeps, WalletServices};
use chargepilot_platform::ids::SegmentAllocator;
use http_body_util::BodyExt;
use chrono::Datelike as _;
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

/// 钱包测试串行锁：资金窗口断言受频次窗口影响，避免并行互扰。
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
    });
    Some((
        chargepilot_http_api::build_router(deps, prometheus_handle(), None),
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

/// 合法 UUID v4（钱包幂等请求号须通过服务端 uuid 校验）。
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
        format!("10.{}.1.{}", rand_suffix(), rand_suffix()),
    );
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn login_user(router: &axum::Router, tag: &str) -> (String, u64) {
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/auth/login",
        Some(json!({"code": format!("dev:{tag}")})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "用户登录失败: {body}");
    let token = body["data"]["jwt"].as_str().unwrap().to_string();
    // user_id 是精确十进制字符串（旧版语义，避免 JS 精度丢失）
    let uid: u64 = body["data"]["user_id"]
        .as_str()
        .expect("user_id 字符串")
        .parse()
        .expect("user_id 可解析");
    (token, uid)
}

/// 建财务角色管理员（绑定风控审核+解冻权限），返回 (token, id)。
async fn login_finance(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    perms: &[&str],
) -> (String, u64) {
    login_finance_as(router, db, alloc, "customer_finance", perms).await
}

/// 以指定内置角色码建管理员（风控审核要求 customer_finance / customer_cs）。
async fn login_finance_as(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    role_code: &str,
    perms: &[&str],
) -> (String, u64) {
    let username = format!("wf-{}", rand_suffix());
    let id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("wf-pass-123").unwrap();
    // 复用既有内置角色（迁移种子已建 customer_finance / customer_cs）。
    let (role_id,): (u64,) = sqlx::query_as(
        "SELECT `id` FROM `role` WHERE `code` = ? AND `deleted_at` IS NULL LIMIT 1",
    )
    .bind(role_code)
    .fetch_one(db)
    .await
    .unwrap();
    
    for p in perms {
        sqlx::query(
            "INSERT IGNORE INTO `role_permission` (`role_id`, `permission_id`) \
             SELECT ?, `id` FROM `permission` WHERE `code` = ?",
        )
        .bind(role_id as i64)
        .bind(p)
        .execute(db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)",
    )
    .bind(id as i64)
    .bind(&username)
    .bind(hash)
    .bind(role_id as i64)
    .execute(db)
    .await
    .unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/login",
        Some(json!({"username": username, "password": "wf-pass-123"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "财务登录失败: {body}");
    let token = body["data"]["access_token"].as_str().unwrap().to_string();
    (token, id)
}

/// 造一笔已到账的微信充值：payment_order(paid) + 充值申请 + 钱包入账（服务级真库事务）。
async fn seed_paid_recharge(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    user_id: u64,
    amount: i64,
) -> u64 {
    let payment_id = alloc.next_id().await.unwrap();
    let month = chrono::Utc::now().date_naive().with_day(1).unwrap();
    sqlx::query(
        "INSERT INTO `payment_order` (`id`, `order_no`, `biz_type`, `biz_id`, `user_id`, \
         `pay_method`, `total_cents`, `paid_cents`, `wechat_transaction_id`, `status`, `paid_at`, `created_month`) \
         VALUES (?, ?, 'wallet_recharge', 0, ?, 'wechat', ?, ?, ?, 'paid', UTC_TIMESTAMP(3), ?)",
    )
    .bind(payment_id as i64)
    .bind(format!("P{}", rand_suffix()))
    .bind(user_id as i64)
    .bind(amount)
    .bind(amount)
    .bind(format!("TESTTXN{}", rand_suffix()))
    .bind(month)
    .execute(db)
    .await
    .unwrap();
    let request_id = format!("seed-{}", rand_suffix());
    let mut tx = db.begin().await.unwrap();
    // 充值申请锁定后入账（模拟回调到账事务）。
    sqlx::query(
        "INSERT INTO `wallet_recharge_request` (`request_id`, `user_id`, `amount_cents`, `payment_order_id`, `request_json`) \
         VALUES (?, ?, ?, ?, '{}')",
    )
    .bind(&request_id)
    .bind(user_id as i64)
    .bind(amount)
    .bind(payment_id as i64)
    .execute(&mut *tx)
    .await
    .unwrap();
    let receipt = chargepilot_mod_wallet::credit::lock_recharge_for_payment_tx(&mut tx, payment_id)
        .await
        .expect("申请锁定");
    assert_eq!(receipt.user_id, user_id);
    chargepilot_mod_wallet::credit::credit_recharge_in_tx(
        &mut tx,
        user_id,
        &receipt.request_id,
        &format!("digest-{:0<64}", rand_suffix()),
        amount,
    )
    .await
    .expect("充值入账");
    tx.commit().await.unwrap();
    payment_id
}


/// 跨测试文件互斥锁：这些 flow 共享开发库，部分断言依赖全局计数
/// （如"最后一个客户管理员"），用固定端口的 TCP 监听作锁串行各文件。
fn global_flow_lock() -> std::net::TcpListener {
    loop {
        if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:39701") {
            return l;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[tokio::test]
async fn wallet_refund_flow() {

    let _flow_lock = global_flow_lock();    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (user_token, uid) = login_user(&router, &format!("wf-{}", rand_suffix())).await;

    // 1. 初始余额：500（充值到账后）
    seed_paid_recharge(&db, &alloc, uid, 500).await;
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/balance",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["balance_cents"], 500, "充值应已入账");
    assert_eq!(body["data"]["available_cents"], 500);

    // 2. 退款申请 300：accepted + 预占拆单
    let request_id = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": request_id, "amount_cents": 300, "reason": "测试退款"})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "退款申请应成功: {body}");
    assert_eq!(body["data"]["status"], "accepted");
    assert_eq!(body["data"]["refund_cents"], 300);
    // 库断言：frozen_cents=300，refund_record 一笔 WR 单，refund_part 一行
    let (frozen, balance): (i64, i64) =
        sqlx::query_as("SELECT `frozen_cents`, `balance_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(uid as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!((frozen, balance), (300, 500), "预占应冻结 300 不动余额");
    let (refund_no, refund_amount, refund_status, policy): (String, i64, String, String) =
        sqlx::query_as(
            "SELECT `refund_no`, `refund_cents`, `status`, `execution_policy` FROM `refund_record` \
             WHERE `user_id` = ? ORDER BY `id` DESC LIMIT 1",
        )
        .bind(uid as i64)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert!(refund_no.starts_with("WR"), "分笔退款号应以 WR 开头: {refund_no}");
    assert_eq!((refund_amount, refund_status.as_str(), policy.as_str()), (300, "pending", "automatic"));

    // 3. 重放同请求：同回执、不重复预占
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": request_id, "amount_cents": 300, "reason": "测试退款"})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放应静默: {body}");
    assert_eq!(body["data"]["status"], "accepted");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM `wallet_refund_part` WHERE `request_id` = ?")
        .bind(&request_id)
        .fetch_one(db.as_ref())
        .await
        .unwrap();
    assert_eq!(n, 1, "重放不重复拆单");

    // 4. 金额不同的重放 → 409
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": request_id, "amount_cents": 100, "reason": "测试退款"})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "金额不同应冲突");

    // 5. 超额申请 → 409（可用余额 500-300=200）
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": rand_uuid(), "amount_cents": 300, "reason": ""})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "超额应拒绝");

    // 6. 频次冻结：造 2 条窗口内历史申请 → 再申请触发 manual_review + 钱包冻结
    for _ in 0..2 {
        let rid = format!("rf-{}", rand_suffix());
        sqlx::query(
            "INSERT INTO `wallet_refund_request` (`request_id`, `user_id`, `amount_cents`, `reason`, `response_json`) \
             VALUES (?, ?, 1, '频次造数', '{}')",
        )
        .bind(&rid)
        .bind(uid as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    }
    let freeze_request = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": freeze_request, "amount_cents": 100, "reason": "第三次"})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "频次申请应受理转人工: {body}");
    assert_eq!(body["data"]["status"], "manual_review", "第 3 次申请应转人工");
    let (wallet_status,): (String,) =
        sqlx::query_as("SELECT `status` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(uid as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(wallet_status, "frozen", "频次冻结应置 frozen");

    // 7. 冻结期间新申请 → 409（Frozen）
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": rand_uuid(), "amount_cents": 50, "reason": ""})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "冻结钱包应拒绝新退款");

    // 8. C 端退款进度视图：manual_review（无审核）
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/refunds",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let view = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["request_id"] == freeze_request.as_str())
        .expect("冻结申请应在列表");
    assert_eq!(view["status"], "manual_review");

    // 9. 无权限管理员 → 403；财务登录 → 列表可见
    let (fin_token, _fin_id) = login_finance(&router, &db, &alloc, &[
        "finance.wallet_risk.review",
        "finance.wallet_risk.release",
    ])
    .await;
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/billing/wallet-risks",
        None,
        Some(&fin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["items"].as_array().unwrap().len() >= 1, "风控列表应有记录");
    // 客服（仅 review 权限）也在列表可见；解冻端点则拒
    let (cs_token, _cs_id) = login_finance_as(&router, &db, &alloc, "customer_cs", &["finance.wallet_risk.review"]).await;
    let (status, _) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/billing/wallet-risks/{freeze_request}/release"),
        Some(json!({"comment": "客服尝试解冻"})),
        Some(&cs_token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "无 release 权限应 403");

    // 10. 审核：批准 → 预占拆单 + 审核回执
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/billing/wallet-risks/{freeze_request}/review"),
        Some(json!({"approved": true, "comment": "核实通过"})),
        Some(&fin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "审核应成功: {body}");
    assert_eq!(body["data"]["approved"], true);
    let (frozen_after,): (i64,) =
        sqlx::query_as("SELECT `frozen_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(uid as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(frozen_after, 400, "批准应追加预占 300+100");
    let (part_n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM `wallet_refund_part` WHERE `request_id` = ?",
    )
    .bind(&freeze_request)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(part_n, 1, "批准应拆单一条");

    // 11. 审核重放：同人同结论同依据 → 同回执
    let (status, body2) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/billing/wallet-risks/{freeze_request}/review"),
        Some(json!({"approved": true, "comment": "核实通过"})),
        Some(&fin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放应静默: {body2}");
    assert_eq!(body["data"], body2["data"]);

    // 12. 解冻：财务解冻 → freeze unfrozen + 钱包恢复 active
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/billing/wallet-risks/{freeze_request}/release"),
        Some(json!({"comment": "风险解除"})),
        Some(&fin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "解冻应成功: {body}");
    assert_eq!(body["data"]["wallet_active"], true);
    let (wallet_status,): (String,) =
        sqlx::query_as("SELECT `status` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(uid as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(wallet_status, "active", "无其他冻结应恢复 active");
    let (freeze_status,): (String,) = sqlx::query_as(
        "SELECT f.`status` FROM `risk_freeze_log` f \
         JOIN `wallet_risk_freeze_link` l ON l.`freeze_id` = f.`id` WHERE l.`request_id` = ?",
    )
    .bind(&freeze_request)
    .fetch_one(db.as_ref())
    .await
    .unwrap();
    assert_eq!(freeze_status, "unfrozen");

    // 13. C 端视图更新：已审核（approved）后不再是 manual_review
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/refunds",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["request_id"] == freeze_request.as_str())
        .expect("申请应在列表");
    // 旧版语义：分笔为 pending（执行器未领取）时展示态保持 pending，processing 只在渠道受理后出现。
    assert_eq!(view["status"], "pending", "已批准在途（分笔 pending）应显示 pending");
    assert!(view["review"]["approved"] == true);

    // 14. 余额查询：frozen 400、available 100
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/balance",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["frozen_cents"], 400);
    assert_eq!(body["data"]["available_cents"], 100);

    // 测试尾部自清理（保留流水供追溯，只删账号）
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'wf-%'")
        .execute(db.as_ref())
        .await;
}

#[tokio::test]
async fn wallet_review_rejection_path() {

    let _flow_lock = global_flow_lock();    let _g = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let (user_token, uid) = login_user(&router, &format!("wf-{}", rand_suffix())).await;
    seed_paid_recharge(&db, &alloc, uid, 100).await;

    // 频次冻结路径：直插 2 条 + 新申请
    for _ in 0..2 {
        let rid = rand_uuid();
        sqlx::query(
            "INSERT INTO `wallet_refund_request` (`request_id`, `user_id`, `amount_cents`, `reason`, `response_json`) \
             VALUES (?, ?, 1, '频次造数', '{}')",
        )
        .bind(&rid)
        .bind(uid as i64)
        .execute(db.as_ref())
        .await
        .unwrap();
    }
    let request_id = rand_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/user/wallet/refund",
        Some(json!({"request_id": request_id, "amount_cents": 50, "reason": "驳回路径"})),
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "manual_review");

    let (fin_token, _fid) = login_finance(&router, &db, &alloc, &[
        "finance.wallet_risk.review",
        "finance.wallet_risk.release",
    ])
    .await;
    // 驳回：不预占，展示 rejected
    let (status, body) = req(
        router.clone(),
        "POST",
        &format!("/api/v1/admin/billing/wallet-risks/{request_id}/review"),
        Some(json!({"approved": false, "comment": "证据不足驳回"})),
        Some(&fin_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "驳回应成功: {body}");
    let (frozen,): (i64,) =
        sqlx::query_as("SELECT `frozen_cents` FROM `wallet_account` WHERE `user_id` = ?")
            .bind(uid as i64)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(frozen, 0, "驳回不预占");
    // C 端视图：rejected
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/wallet/refunds",
        None,
        Some(&user_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["request_id"] == request_id.as_str())
        .expect("申请应在列表");
    assert_eq!(view["status"], "rejected", "驳回后应显示 rejected");

    // 测试尾部自清理
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'wf-%'")
        .execute(db.as_ref())
        .await;
}
