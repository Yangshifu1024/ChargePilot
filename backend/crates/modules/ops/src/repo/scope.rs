//! 数据范围与字段脱敏表访问（旧版 scope_repo.go 的单体对应物）：
//! 读取 admin_data_scope / admin_field_mask 并归并为 domain 内存集合。
//! 锁定读（FOR SHARE）供导出等发布路径保证文件与保存的规则一致。

use crate::Profile;
use chargepilot_domain::operations::{self, DataScope, FieldMask};
use sqlx::MySqlConnection;

/// 读取并合并账号范围；shared=true 时 FOR SHARE（须在事务内调用）。
/// 无记录返回 unrestricted=true（旧版 LoadDataScope / LockDataScopeInTx）。
pub async fn load_data_scope_tx(
    c: &mut MySqlConnection,
    profile: &Profile,
    shared: bool,
) -> anyhow::Result<DataScope> {
    let sql = format!(
        "SELECT `scope_type`, `scope_id` FROM `admin_data_scope` WHERE `admin_user_id` = ?{}",
        if shared { " FOR SHARE" } else { "" }
    );
    let rows: Vec<(chargepilot_domain::operations::AdminScopeType, u64)> = sqlx::query_as(&sql)
        .bind(profile.id as i64)
        .fetch_all(&mut *c)
        .await?;
    Ok(operations::new_data_scope(&rows))
}

/// 读取角色脱敏规则；role_id=0 返回空规则，不脱敏
/// （旧版 LoadFieldMask / LockFieldMaskInTx）。
pub async fn load_field_mask_tx(
    c: &mut MySqlConnection,
    role_id: u64,
    shared: bool,
) -> anyhow::Result<FieldMask> {
    let mut mask = FieldMask {
        role_id,
        fields: vec![],
    };
    if role_id == 0 {
        return Ok(mask);
    }
    let sql = format!(
        "SELECT `resource`, `field` FROM `admin_field_mask` WHERE `role_id` = ?{}",
        if shared { " FOR SHARE" } else { "" }
    );
    let rows: Vec<(String, String)> = sqlx::query_as(&sql)
        .bind(role_id as i64)
        .fetch_all(&mut *c)
        .await?;
    mask.fields = rows;
    Ok(mask)
}
