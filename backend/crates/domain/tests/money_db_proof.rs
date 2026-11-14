//! 票 06 真库打样：money 词表枚举（`wordlist_enum!` 手写 sqlx 三件套）对真实
//! MySQL ENUM 语义的绑定与解码证明。用临时表复刻 `payment_order.status` 的
//! ENUM DDL，不触碰业务表：① DEFAULT 行解码（DB 写默认值 → Rust 枚举）；
//! ② 枚举绑定 UPDATE；③ Option 包裹 NULL/Some 解码；④ 真实业务表绑定兼容。
//! 背景结论：`#[derive(sqlx::Type)]` 派生强枚举的 Decode 对 MySQL ENUM 列
//! 结构性失败（compatible 严格相等永不命中），故三件套手写——
//! 详见 docs/design/enum-mapping.md 总则 1 的派生更正。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use chargepilot_domain::money::PaymentOrderStatus;

#[ignore = "真库集成：node tasks.mjs test-integration"]
#[tokio::test]
#[allow(clippy::disallowed_methods)] // 票 16 豁免：测试模块的连接变量
async fn enum_bind_decode_default_and_option() {
    let url = std::env::var("TEST_MYSQL_URL").unwrap_or_else(|_| {
        panic!("集成测试需要 TEST_MYSQL_URL（node tasks.mjs test-integration）")
    });
    let pool = sqlx::MySqlPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("数据库连接失败：{e}"));
    let mut conn = pool.acquire().await.expect("取连接");

    // 真实 ENUM DDL（与 payment_order.status 逐字节同词表），NOT NULL + DEFAULT。
    sqlx::query(
        "CREATE TEMPORARY TABLE _cp_enum_proof (\
             status ENUM('initiated','paid','closed','refunded','partial_refunded','failed')\
             NOT NULL DEFAULT 'initiated')",
    )
    .execute(&mut *conn)
    .await
    .expect("建临时表");

    // ① DEFAULT 行：空插入走列默认，解码应得 Initiated。
    sqlx::query("INSERT INTO _cp_enum_proof () VALUES ()")
        .execute(&mut *conn)
        .await
        .expect("默认插入");
    let (decoded,): (PaymentOrderStatus,) = sqlx::query_as("SELECT status FROM _cp_enum_proof")
        .fetch_one(&mut *conn)
        .await
        .expect("默认值解码");
    assert_eq!(
        decoded,
        PaymentOrderStatus::Initiated,
        "DEFAULT 'initiated' 必须解码为枚举"
    );

    // ② 枚举直接绑定 UPDATE（DB→驱动按字符串编码，与绑定 &str 等价）。
    sqlx::query("UPDATE _cp_enum_proof SET status = ?")
        .bind(PaymentOrderStatus::PartialRefunded)
        .execute(&mut *conn)
        .await
        .expect("枚举绑定");
    let (updated,): (PaymentOrderStatus,) = sqlx::query_as("SELECT status FROM _cp_enum_proof")
        .fetch_one(&mut *conn)
        .await
        .expect("绑定后解码");
    assert_eq!(updated, PaymentOrderStatus::PartialRefunded);

    // ③ Option 包裹：NULL 行解码为 None；非 NULL 行为 Some。
    sqlx::query("ALTER TABLE _cp_enum_proof MODIFY status ENUM('initiated','paid','closed','refunded','partial_refunded','failed') NULL")
        .execute(&mut *conn)
        .await
        .expect("改可空");
    sqlx::query("UPDATE _cp_enum_proof SET status = NULL")
        .execute(&mut *conn)
        .await
        .expect("写 NULL");
    let (none_case,): (Option<PaymentOrderStatus>,) =
        sqlx::query_as("SELECT status FROM _cp_enum_proof")
            .fetch_one(&mut *conn)
            .await
            .expect("NULL 解码");
    assert_eq!(none_case, None, "NULL 必须解码为 None");
    sqlx::query("UPDATE _cp_enum_proof SET status = 'paid'")
        .execute(&mut *conn)
        .await
        .expect("回写 paid");
    let (some_case,): (Option<PaymentOrderStatus>,) =
        sqlx::query_as("SELECT status FROM _cp_enum_proof")
            .fetch_one(&mut *conn)
            .await
            .expect("Some 解码");
    assert_eq!(some_case, Some(PaymentOrderStatus::Paid));

    // ④ 对真实业务表绑定（不写行）：证明与既有列类型兼容。
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM `payment_order` WHERE `status` = ?")
            .bind(PaymentOrderStatus::Initiated)
            .fetch_one(&pool)
            .await
            .expect("业务表枚举绑定");
    let _ = count; // 计数值不敏感，能执行即证明绑定兼容

    sqlx::query("DROP TEMPORARY TABLE _cp_enum_proof")
        .execute(&mut *conn)
        .await
        .expect("清理");
    pool.close().await;
}
