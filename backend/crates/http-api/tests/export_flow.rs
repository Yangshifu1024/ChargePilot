//! 阶段 05 批 5 主缝集成测试：导出受理（同步生成）→ 列表/详情 → CSV/XLSX
//! 下载（表头与行数，不逐字节比对）→ 同请求号重放不换任务号 → 换参数冲突
//! → 受限范围拒绝账单导出、订单经设备表过滤 → 到期后下载 410 并即时清理
//! → 定时清理回收文件。管理端走 HTTP，到期清理直调模块函数（旧版定时循环
//! 调用缝）。EXPORT_DIR 是进程级环境变量，全部场景必须在同一测试内串行完成。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::ApiDeps;
use chargepilot_mod_export::cleanup_expired;
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

/// 导出域测试串行锁（共享 dev 库的 flow 测试互斥；EXPORT_DIR 环境变量同样
/// 是进程级状态，必须在锁内改读）。
static FLOW_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn build() -> Option<(axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>)> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let Some(db_url) = env_or_skip("TEST_MYSQL_URL") else {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）");
    };
    let Some(redis_url) = env_or_skip("TEST_REDIS_URL") else {
        panic!("集成测试需要 TEST_REDIS_URL（node tasks.mjs test-integration）");
    };
    let db = match chargepilot_storage::open_pool(&db_url, 8).await {
        Ok(d) => d,
        Err(e) => {
            panic!("数据库连接失败：{e}");
        }
    };
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 60));
    if let Err(e) = allocator.warm_up().await {
        panic!("号段预热失败：{e}");
    }
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        snapshots: None,
        charging_stop: None,
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

/// 生成 UUID 形态的请求号（8-4-4-4-12 十六进制），前缀固定便于清理。
fn flow_uuid() -> String {
    let (s1, s2, s3) = (rand_suffix(), rand_suffix(), rand_suffix());
    let tail: String = format!("{s1}{s2}").chars().take(12).collect();
    format!(
        "ee000000-{a}-4{b}-8{c}-{tail}",
        a = &s1[..4],
        b = &s2[..3],
        c = &s3[..3]
    )
}

/// 原始响应（下载断言需要状态行、头与字节体，不走 JSON 解析）。
async fn req_raw(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn body_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// 重查任务详情，返回 row_count（下载行数核对用）。
async fn body_json_lookup(router: &axum::Router, token: &str, task_id: u64) -> i64 {
    let (_, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/exports/{task_id}"),
        None,
        Some(token),
    )
    .await;
    body["data"]["row_count"].as_i64().unwrap()
}

async fn req(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _, bytes) = req_raw(router, method, path, body, token).await;
    (status, body_json(&bytes))
}

/// 建独立角色 + 权限的管理员并登录，返回 (token, admin_id, role_id)。
async fn login_export_admin(
    router: &axum::Router,
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    perms: &[&str],
) -> (String, u64, u64) {
    let username = format!("ex-{}", rand_suffix());
    let (role_id, id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '导出测试角色', 0)",
    )
    .bind(role_id as i64)
    .bind(format!("ex_{username}"))
    .execute(db)
    .await
    .unwrap();
    for p in perms {
        sqlx::query(
            "INSERT INTO `role_permission` (`role_id`, `permission_id`) \
             SELECT ?, `id` FROM `permission` WHERE `code` = ?",
        )
        .bind(role_id as i64)
        .bind(p)
        .execute(db)
        .await
        .unwrap();
    }
    let hash = chargepilot_platform::password::hash("ex-pass-123").unwrap();
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
        Some(json!({"username": username, "password": "ex-pass-123"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "管理员登录失败: {body}");
    let token = body["data"]["access_token"].as_str().unwrap().to_string();
    (token, id, role_id)
}

/// 种子两条可导出订单（设备号带随机前缀，清理只删本运行的数据）。
async fn seed_orders(db: &sqlx::MySqlPool, count: u32, prefix: &str) {
    for i in 0..count {
        sqlx::query(
            "INSERT INTO `charge_order` (`order_no`, `user_id`, `device_id`, `port_no`, \
             `status`, `created_month`) VALUES (?, 1, ?, ?, 'completed', '2026-11-01')",
        )
        .bind(format!("EXPTEST20261114{i:04}{:08}", rand_suffix()))
        .bind(format!("{prefix}-{i}"))
        .bind(i + 1)
        .execute(db)
        .await
        .unwrap();
    }
}

/// 尾部自清理（任务、审计、订单、范围、管理员/角色、导出目录）。
/// 任务与审计按精确集合删除，避免与共享 dev 库的历史数据互删。
async fn cleanup(
    db: &sqlx::MySqlPool,
    export_dir: &str,
    task_nos: &[String],
    order_prefix: &str,
    admin_ids: &[u64],
    role_ids: &[u64],
) {
    for task_no in task_nos {
        let _ = sqlx::query("DELETE FROM `export_task` WHERE `task_no` = ?")
            .bind(task_no)
            .execute(db)
            .await;
    }
    for id in admin_ids {
        let _ = sqlx::query(
            "DELETE FROM `audit_log` WHERE `target_type` = 'export_task' AND `actor_id` = ?",
        )
        .bind(*id as i64)
        .execute(db)
        .await;
    }
    let _ = sqlx::query("DELETE FROM `charge_order` WHERE `device_id` LIKE ?")
        .bind(format!("{order_prefix}-%"))
        .execute(db)
        .await;
    for id in admin_ids {
        let _ = sqlx::query("DELETE FROM `admin_data_scope` WHERE `admin_user_id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `admin_user` WHERE `id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
    }
    for id in role_ids {
        let _ = sqlx::query("DELETE FROM `role_permission` WHERE `role_id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `admin_field_mask` WHERE `role_id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM `role` WHERE `id` = ?")
            .bind(*id as i64)
            .execute(db)
            .await;
    }
    let _ = std::fs::remove_dir_all(export_dir);
}

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
async fn export_lifecycle_csv_xlsx_replay_and_expiry() {
    let _guard = FLOW_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    let export_dir = std::env::temp_dir()
        .join(format!("chargepilot-export-test-{}", rand_suffix()))
        .to_string_lossy()
        .into_owned();
    // 进程级配置：主缝路由每次请求时读取（旧版 ExportDir 环境变量语义）。
    unsafe { std::env::set_var("EXPORT_DIR", &export_dir) };
    let (token_a, admin_a, role_a) = login_export_admin(
        &router,
        &db,
        &alloc,
        &["export.create", "order.read", "finance.read"],
    )
    .await;

    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM `charge_order` WHERE `deleted_at` IS NULL")
            .fetch_one(&*db)
            .await
            .unwrap();
    let order_prefix = format!("expflow-{}", rand_suffix());
    seed_orders(&db, 2, &order_prefix).await;

    // 资源清单：登记资源与格式、行上限。
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/exports/resources",
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "资源清单失败: {body}");
    let items: Vec<String> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(items.contains(&"orders".to_string()), "资源清单: {items:?}");
    assert_eq!(body["data"]["formats"], json!(["csv", "xlsx"]));

    // 受理 orders 导出（同步生成 completed）。
    let mut task_nos: Vec<String> = Vec::new();
    let request_id = flow_uuid();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": request_id, "resource": "orders", "format": "csv"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "受理导出失败: {body}");
    assert_eq!(
        body["data"]["status"], "completed",
        "同步生成应完成: {body}"
    );
    assert_eq!(
        body["data"]["row_count"],
        json!(before + 2),
        "导出行数 = 存量+种子: {body}"
    );
    let task_no = body["data"]["task_no"].as_str().unwrap().to_string();
    assert!(task_no.starts_with("EXP"));
    task_nos.push(task_no.clone());
    let task_id: u64 = sqlx::query_scalar("SELECT `id` FROM `export_task` WHERE `task_no` = ?")
        .bind(&task_no)
        .fetch_one(&*db)
        .await
        .unwrap();
    // 内容文件与校验文件均已发布。
    assert!(
        std::path::Path::new(&export_dir)
            .join("v1")
            .join(format!("{task_no}.csv"))
            .exists()
    );
    assert!(
        std::path::Path::new(&export_dir)
            .join("v1")
            .join(format!("{task_no}.scope.json"))
            .exists()
    );

    // 详情与列表。
    let (status, body) = req(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/exports/{task_id}"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "详情失败: {body}");
    assert_eq!(body["data"]["status"], "completed");
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/exports?status=completed&page=1&page_size=20",
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "列表失败: {body}");
    assert!(body["data"]["total"].as_i64().unwrap() >= 1);

    // CSV 下载：中文表头 + 表头行 + 数据行数与 row_count 一致。
    let (status, headers, bytes) = req_raw(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/exports/{task_id}/download"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CSV 下载失败");
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/csv"),
        "content-type: {:?}",
        headers["content-type"]
    );
    let text = String::from_utf8(bytes.clone()).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "订单号,用户,设备,端口,状态,开始时间,结束时间,电费(分),服务费(分),合计(分)"
    );
    let row_count = body_json_lookup(&router, &token_a, task_id).await;
    assert_eq!(
        lines.count(),
        row_count as usize,
        "CSV 数据行数应等于 row_count"
    );

    // 同请求号重放：同任务号、不重复审计、不重复生成。
    async fn audits_of(db: &sqlx::MySqlPool, admin: u64) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM `audit_log` WHERE `target_type` = 'export_task' AND `actor_id` = ?",
        )
        .bind(admin as i64)
        .fetch_one(db)
        .await
        .unwrap()
    }
    let audits_before = audits_of(&db, admin_a).await;
    let (status, replay) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": request_id, "resource": "orders", "format": "csv"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "重放失败: {replay}");
    assert_eq!(replay["data"]["task_no"], json!(task_no), "重放须同任务号");
    assert_eq!(
        audits_of(&db, admin_a).await,
        audits_before,
        "重放不得重复审计"
    );

    // 同请求号换参数（同资源换格式）：重放比对失败 409 冲突。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": request_id, "resource": "orders", "format": "xlsx"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "换参数应冲突: {body}");

    // XLSX 导出：PK(Zip) 魔数 + 专用 MIME；行列数不逐字节比对。
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": flow_uuid(), "resource": "orders", "format": "xlsx"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "XLSX 受理失败: {body}");
    assert_eq!(body["data"]["status"], "completed");
    let xlsx_no = body["data"]["task_no"].as_str().unwrap().to_string();
    task_nos.push(xlsx_no.clone());
    let xlsx_id: u64 = sqlx::query_scalar("SELECT `id` FROM `export_task` WHERE `task_no` = ?")
        .bind(&xlsx_no)
        .fetch_one(&*db)
        .await
        .unwrap();
    let (status, headers, bytes) = req_raw(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/exports/{xlsx_id}/download"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "XLSX 下载失败");
    assert_eq!(
        headers["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(&bytes[..2], b"PK", "XLSX 应为 Zip 容器");

    // 无效格式与未登记资源：400。
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": flow_uuid(), "resource": "orders", "format": "pdf"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": flow_uuid(), "resource": "secret", "format": "csv"})),
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 到期任务下载：410 并即时清理（状态 expired + 文件删除）。
    sqlx::query(
        "UPDATE `export_task` SET `expires_at` = NOW(3) - INTERVAL 1 HOUR WHERE `task_no` = ?",
    )
    .bind(&task_no)
    .execute(&*db)
    .await
    .unwrap();
    let (status, _, _) = req_raw(
        router.clone(),
        "GET",
        &format!("/api/v1/admin/exports/{task_id}/download"),
        None,
        Some(&token_a),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "到期下载应 410");
    let expired_status: String =
        sqlx::query_scalar("SELECT `status` FROM `export_task` WHERE `task_no` = ?")
            .bind(&task_no)
            .fetch_one(&*db)
            .await
            .unwrap();
    assert_eq!(expired_status, "expired", "下载到期应即时标记 expired");
    assert!(
        !std::path::Path::new(&export_dir)
            .join("v1")
            .join(format!("{task_no}.csv"))
            .exists(),
        "到期文件应已删除"
    );

    // 定时清理（旧版循环调用缝）：XLSX 任务到期后清理返回 1 并删文件。
    sqlx::query(
        "UPDATE `export_task` SET `expires_at` = NOW(3) - INTERVAL 1 HOUR WHERE `task_no` = ?",
    )
    .bind(&xlsx_no)
    .execute(&*db)
    .await
    .unwrap();
    let removed = cleanup_expired(&db, &export_dir).await.unwrap();
    assert_eq!(removed, 1, "应清理 1 个到期任务");
    assert!(
        !std::path::Path::new(&export_dir)
            .join("v1")
            .join(format!("{xlsx_no}.xlsx"))
            .exists(),
        "清理后文件应删除"
    );
    let xlsx_status: String =
        sqlx::query_scalar("SELECT `status` FROM `export_task` WHERE `task_no` = ?")
            .bind(&xlsx_no)
            .fetch_one(&*db)
            .await
            .unwrap();
    assert_eq!(xlsx_status, "expired");
    // 重复执行不重复计数。
    let removed = cleanup_expired(&db, &export_dir).await.unwrap();
    assert_eq!(removed, 0, "重复清理不得重复计数");

    // 受限数据范围：账单只允许全局范围（403）；订单经 device_meta 子查询
    // 过滤为空集——修正旧版直接拼 charge_order.station_id 报未知列的缺陷。
    let (token_b, admin_b, role_b) = login_export_admin(
        &router,
        &db,
        &alloc,
        &["export.create", "order.read", "finance.read"],
    )
    .await;
    sqlx::query(
        "INSERT INTO `admin_data_scope` (`admin_user_id`, `scope_type`, `scope_id`, `created_by`) \
         VALUES (?, 'station', 987654321, ?)",
    )
    .bind(admin_b as i64)
    .bind(admin_b as i64)
    .execute(&*db)
    .await
    .unwrap();
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": flow_uuid(), "resource": "bills",
                    "format": "csv",
                    "filter": {"from": "2026-11-01", "to": "2026-11-14"}})),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "受限账单导出应 403: {body}");
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/exports",
        Some(json!({"request_id": flow_uuid(), "resource": "orders", "format": "csv"})),
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "受限订单导出应成功: {body}");
    assert_eq!(body["data"]["status"], "completed", "{body}");
    assert_eq!(body["data"]["row_count"], 0, "受限范围应为 0 行: {body}");
    task_nos.push(body["data"]["task_no"].as_str().unwrap().to_string());
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/exports/resources",
        None,
        Some(&token_b),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items: Vec<String> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        !items.contains(&"bills".to_string()),
        "受限范围不含 bills: {items:?}"
    );

    cleanup(
        &db,
        &export_dir,
        &task_nos,
        &order_prefix,
        &[admin_a, admin_b],
        &[role_a, role_b],
    )
    .await;
}
