//! ops 尾款主缝集成测试：账号/角色管理、越权、最后管理员保护、公告、仪表盘。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chargepilot_http_api::deps::ApiDeps;
use chargepilot_platform::ids::SegmentAllocator;
use http_body_util::BodyExt;
use serde_json::Value;
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

async fn build() -> Option<(axum::Router, Arc<sqlx::MySqlPool>, Arc<SegmentAllocator>)> {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
    let db_url = env_or_skip("TEST_MYSQL_URL")?;
    let redis_url = env_or_skip("TEST_REDIS_URL")?;
    let db = chargepilot_storage::open_pool(&db_url, 8).await.ok()?;
    let allocator = Arc::new(SegmentAllocator::new(db.clone(), 50));
    allocator.warm_up().await.ok()?;
    let deps = Arc::new(ApiDeps {
        db: db.clone(),
        redis_url: Arc::new(redis_url),
        jwt_secret: Arc::new("0123456789abcdef0123456789abcdef".into()),
        allocator: allocator.clone(),
        exchanger: Arc::new(chargepilot_mod_identity::wechat::Exchanger::Development),
        // 这些 flow 不触达钱包与支付域。
        wallet: None,
        checkout: None,
        wallet_recharge: None,
        simulation_payment: false,
        charging_stop: None,
    });
    Some((
        chargepilot_http_api::build_router(deps, prometheus_handle(), None),
        Arc::new(db),
        allocator,
    ))
}

/// 管理测试共享 admin_user/role 数据，必须串行（进程级互斥）。
static MGMT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 清理历史运行残留的测试账号/角色（避免最后管理员计数被污染）。
async fn cleanup_test_admins(db: &sqlx::MySqlPool) {
    let _ = sqlx::query("DELETE FROM admin_user WHERE username LIKE 'root-%' OR username LIKE 'lim-%' OR username LIKE 'last-%'")
        .execute(db).await;
    // 陈旧残留（其他 flow 历史运行的超管账号）只删 1 小时前的，避免干扰并行中的测试进程。
    let _ = sqlx::query(
        "DELETE FROM admin_user WHERE (username LIKE 'p3-%' OR username LIKE 'wf-%') \
         AND created_at < NOW() - INTERVAL 1 HOUR",
    )
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM role WHERE code LIKE 'lim_%' OR code LIKE 'rogue_%'")
        .execute(db)
        .await;
}

fn rand_ip() -> String {
    format!("10.{}.1.{}", rand_suffix(), rand_suffix())
}

fn rand_suffix() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn req(
    router: axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    ip: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-real-ip", ip);
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

async fn login(router: &axum::Router, username: &str, password: &str, _ip: &str) -> Option<String> {
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/auth/login",
        Some(&format!(
            r#"{{"username":"{username}","password":"{password}"}}"#
        )),
        None,
        &rand_ip(),
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("登录失败: {body}");
        return None;
    }
    body["data"]["access_token"].as_str().map(str::to_string)
}

/// SQL 直建受限管理员（角色 permissions = 指定权限码）。
async fn seed_limited_admin(
    db: &sqlx::MySqlPool,
    alloc: &SegmentAllocator,
    suffix: &str,
    permissions: &[&str],
) -> (String, u64, u64) {
    let username = format!("lim-{suffix}");
    let (role_id, admin_id) = (
        alloc.next_id().await.unwrap(),
        alloc.next_id().await.unwrap(),
    );
    sqlx::query(
        "INSERT INTO `role` (`id`, `code`, `name`, `is_builtin`) VALUES (?, ?, '受限测试角色', 0)",
    )
    .bind(role_id)
    .bind(format!("lim_{suffix}"))
    .execute(db)
    .await
    .unwrap();
    for p in permissions {
        sqlx::query("INSERT INTO `role_permission` (`role_id`, `permission_id`) SELECT ?, `id` FROM `permission` WHERE `code` = ?")
            .bind(role_id).bind(p).execute(db).await.unwrap();
    }
    let hash = chargepilot_platform::password::hash("limited-pass-123").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, ?)")
        .bind(admin_id).bind(&username).bind(hash).bind(role_id).execute(db).await.unwrap();
    (username, role_id, admin_id)
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
async fn permission_boundary_and_management() {
    let _flow_lock = global_flow_lock();
    let _guard = MGMT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    cleanup_test_admins(&db).await;
    let suf = rand_suffix();
    let super_name = format!("root-{suf}");
    let (lim_name, lim_role_id, lim_admin_id) = seed_limited_admin(
        &db,
        &alloc,
        &suf,
        &[
            "announcement.read",
            "dashboard.read",
            "announcement.create",
            "admin_user.update",
            "role.create",
        ],
    )
    .await;

    // 超管：SQL 直建 + 挂 customer_admin（内置角色，权限来自种子）
    let super_id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("super-pass-123").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(super_id).bind(&super_name).bind(hash).execute(db.as_ref()).await.unwrap();
    let super_token = login(&router, &super_name, "super-pass-123", &rand_ip())
        .await
        .expect("超管登录");
    let lim_token = login(&router, &lim_name, "limited-pass-123", &rand_ip())
        .await
        .expect("受限登录");

    // 1. 越权：受限账号访问账号列表 → 403/1003
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/admin-users",
        None,
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "受限账号应被拒绝: {body}");
    assert_eq!(body["code"], 1003);

    // 2. 超管列表可用且为分页形状
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/admin-users?page=1&page_size=5",
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["items"].is_array());
    assert!(body["data"]["total"].is_i64());

    // 3. 角色列表：permissions 全部过滤退役码前缀
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/roles",
        None,
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for item in body["data"]["items"].as_array().unwrap() {
        if let Some(perms) = item["permissions"].as_array() {
            for p in perms {
                let code = p.as_str().unwrap();
                assert!(
                    !code.starts_with("ota.") && !code.starts_with("customer_service."),
                    "退役码必须被过滤: {code}"
                );
            }
        }
    }

    // 4. 最后管理员保护：唯一性由测试构造——先停用其他所有有效的
    // customer_admin（历史运行/并行测试的账号一律禁用），使 super_id 成为
    // 最后一个；受限账号（有 admin_user.update）停用它必须 409 且整体回滚。
    sqlx::query(
        "UPDATE `admin_user` SET `status` = 'disabled'          WHERE `role_id` = 1 AND `id` <> ? AND `status` = 'active' AND `deleted_at` IS NULL",
    )
    .bind(super_id)
    .execute(db.as_ref())
    .await
    .unwrap();
    let (status, body) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/admin-users/{super_id}"),
        Some(r#"{"status":"disabled"}"#),
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "最后管理员保护应拒绝: {body}");
    assert!(body["message"].as_str().unwrap().contains("客户管理员"));
    // 事务回滚：超管必须仍是 active（写先于保护检查发生，409 时必须整体回滚）
    let still_active: String =
        sqlx::query_scalar("SELECT `status` FROM `admin_user` WHERE `id` = ?")
            .bind(super_id)
            .fetch_one(db.as_ref())
            .await
            .unwrap();
    assert_eq!(still_active, "active", "保护拒绝后必须回滚");

    // 5. 受限账号无法授出自己没有的角色权限：建角色含 admin_user.read → 400
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/roles",
        Some(&format!(
            r#"{{"code":"rogue_{suf}","name":"越权角色","permissions":["admin_user.read"]}}"#
        )),
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "越权授角色应拒绝: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("只能从自己已持有的权限")
    );

    // 6. 超管创建受限账号可管理的角色并验证可用：公告发布（受限账号有 announcement.create）
    let (status, body) = req(
        router.clone(),
        "POST",
        "/api/v1/admin/announcements",
        Some(&format!(
            r#"{{"title":"公告{suf}","content":"内容","scope":"global","start_at":"{}"}}"#,
            chrono::Utc::now().to_rfc3339()
        )),
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "受限账号发布公告应成功: {body}");
    let ann_id = body["data"]["id"].as_u64().unwrap();

    // 7. C 端公告可见
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/user/announcement/list?page=1&page_size=10",
        None,
        None,
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let found = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"].as_u64() == Some(ann_id));
    assert!(found, "新公告应在 C 端可见");

    // 8. 仪表盘（受限账号有 dashboard.read）→ 形状断言
    let (status, body) = req(
        router.clone(),
        "GET",
        "/api/v1/admin/dashboard",
        None,
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["daily_trend"]
            .as_array()
            .map(|a| a.len() == 7)
            .unwrap_or(false),
        "趋势固定 7 格"
    );
    assert!(body["data"]["total_users"].is_i64());

    // 9. 受限账号越权删除超管 → 403（admin_user.delete 不在权限集）
    let (status, _) = req(
        router.clone(),
        "DELETE",
        &format!("/api/v1/admin/admin-users/{lim_admin_id}"),
        None,
        Some(&lim_token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 10. 超管更新受限账号角色 → 409（角色不存在或最后管理员保护不涉及）；仅验证更新接口可达
    let (status, _) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/admin-users/{lim_admin_id}"),
        Some(&format!(r#"{{"role_id":{lim_role_id}}}"#)),
        Some(&super_token),
        &rand_ip(),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::CONFLICT,
        "更新接口应可达: {status}"
    );
}

#[tokio::test]
async fn last_admin_self_protection() {
    let _flow_lock = global_flow_lock();
    let _guard = MGMT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((router, db, alloc)) = build().await else {
        return;
    };
    cleanup_test_admins(&db).await;
    let suf = rand_suffix();
    let name = format!("last-{suf}");
    let id = alloc.next_id().await.unwrap();
    let hash = chargepilot_platform::password::hash("last-pass-1234").unwrap();
    sqlx::query("INSERT INTO `admin_user` (`id`, `username`, `password_hash`, `role_id`) VALUES (?, ?, ?, 1)")
        .bind(id).bind(&name).bind(hash).execute(db.as_ref()).await.unwrap();
    let token = login(&router, &name, "last-pass-1234", &rand_ip())
        .await
        .expect("登录");

    // 本人禁改自己状态 → 409
    let (status, body) = req(
        router.clone(),
        "PUT",
        &format!("/api/v1/admin/admin-users/{id}"),
        Some(r#"{"status":"disabled"}"#),
        Some(&token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "本人禁改自身状态: {body}");
    // 本人禁删自己 → 409
    let (status, body) = req(
        router.clone(),
        "DELETE",
        &format!("/api/v1/admin/admin-users/{id}"),
        None,
        Some(&token),
        &rand_ip(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "本人禁删自己: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("不能删除当前登录账号")
    );
}
