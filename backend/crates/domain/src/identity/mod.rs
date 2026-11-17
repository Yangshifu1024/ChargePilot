//! 身份域共享词表：C 端用户状态与性别。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// user.status 列的 C 端用户状态（两值；与后台
    /// [`crate::operations::AdminUserStatus`] 是不同表的独立词表，不合并）。
    UserStatus: {
        Active = "active",
        Frozen = "frozen",
    }
);

wordlist_enum!(
    /// user.gender 列的用户性别（三值；注册不写入，DB DEFAULT 'unknown'）。
    UserGender: {
        Unknown = "unknown",
        Male = "male",
        Female = "female",
    }
);

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let status = [
            (UserStatus::Active, "active"),
            (UserStatus::Frozen, "frozen"),
        ];
        for (v, s) in status {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let gender = [
            (UserGender::Unknown, "unknown"),
            (UserGender::Male, "male"),
            (UserGender::Female, "female"),
        ];
        for (v, s) in gender {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
