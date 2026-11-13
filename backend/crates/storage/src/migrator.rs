//! 自研极简迁移器：按文件名序执行、`schema_migrations` 记录版本、失败即停。

use sqlx::MySqlPool;

/// 编译期内嵌的迁移基线（新增迁移按文件名序追加）。
pub const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_init.sql",
    include_str!("../../../migrations/0001_init.sql"),
)];

/// 对目标库执行全部未应用迁移，返回本次应用的版本号列表。
pub async fn run_migrations(pool: &MySqlPool) -> anyhow::Result<Vec<u64>> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS `schema_migrations` (\
           `version` bigint unsigned NOT NULL,\
           `name` varchar(255) COLLATE utf8mb4_unicode_ci NOT NULL,\
           `applied_at` datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\
           PRIMARY KEY (`version`)\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    )
    .execute(pool)
    .await?;

    let mut applied = Vec::new();
    for (idx, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as u64;
        let done: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM `schema_migrations` WHERE `version` = ?")
                .bind(version)
                .fetch_optional(pool)
                .await?;
        if done.is_some() {
            tracing::info!(version, name, "迁移已应用，跳过");
            continue;
        }
        // 逐条执行（MySQL DDL 隐式提交；任一语句失败即停止，不记版本）
        for (stmt_idx, stmt) in split_statements(sql).iter().enumerate() {
            sqlx::query(stmt).execute(pool).await.map_err(|e| {
                anyhow::anyhow!(
                    "迁移 {name} 第 {stmt_idx} 条语句失败: {e}\n语句: {}",
                    truncate(stmt)
                )
            })?;
        }
        sqlx::query("INSERT INTO `schema_migrations` (`version`, `name`) VALUES (?, ?)")
            .bind(version)
            .bind(*name)
            .execute(pool)
            .await?;
        tracing::info!(version, name, "迁移应用完成");
        applied.push(version);
    }
    Ok(applied)
}

/// 按「分号 + 换行」边界切分语句；0001_init.sql 已验证全部段引号平衡。
fn split_statements(sql: &str) -> Vec<String> {
    sql.split(";\n")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '\n' || c == '-' || c == ' '))
        .collect()
}

fn truncate(s: &str) -> String {
    let t: String = s.chars().take(120).collect();
    if s.chars().count() > 120 {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_splits_into_balanced_statements() {
        let (_, sql) = MIGRATIONS[0];
        let stmts = split_statements(sql);
        assert!(stmts.len() > 200, "语句数过少: {}", stmts.len());
        for s in &stmts {
            let quotes = s.matches('\'').count();
            assert_eq!(quotes % 2, 0, "引号不平衡的语句: {s:.}");
        }
        // 段首可能带前导注释行，用包含匹配统计建表语句
        let creates = stmts
            .iter()
            .filter(|s| s.to_uppercase().contains("CREATE TABLE `"))
            .count();
        assert_eq!(
            creates, 84,
            "CREATE TABLE 数量应为 84（见 docs/design/schema.md）"
        );
    }
}
