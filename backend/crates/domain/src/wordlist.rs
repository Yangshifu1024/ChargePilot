//! 词表枚举生成宏（全域共享）。
//!
//! sqlx 三件套为什么手写：sqlx 0.8.6 派生强枚举只生成
//! `type_info = MySqlTypeInfo::__enum`（type=String + ENUM 标志，
//! 为参数绑定设计），不覆写 `compatible`，解码走默认严格类型相等——
//! 而 MySQL 结果集把 ENUM 列报告为 `ColumnType::Enum`，两者永不相等，
//! 连 `CAST AS CHAR`（VarString）也被拒。因此兼容判定委托
//! `<str>::compatible`（接受 Enum 列且非 BINARY），解码按字面量 parse，
//! 未知值即 schema 漂移报警（严格失败）。

/// 词表枚举生成（跨 crate 经 `chargepilot_domain::wordlist_enum!` 使用；
/// crate 内部沿用既有路径）。
#[macro_export]
macro_rules! wordlist_enum {
    (
        $(#[$meta:meta])*
        $name:ident : { $( $variant:ident = $literal:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $( $variant ),+ }

        impl $name {
            /// DB 字面量锚点（与 `migrations/0001_init.sql` 逐字节一致）。
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $literal ),+ }
            }

            /// DB/serde 字面量 → 枚举；未知值报词表名。
            pub fn parse(value: &str) -> Result<Self, $crate::money::UnknownLiteral> {
                match value {
                    $( $literal => Ok(Self::$variant), )+
                    other => Err($crate::money::UnknownLiteral {
                        wordlist: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl sqlx::Type<sqlx::MySql> for $name {
            fn type_info() -> sqlx::mysql::MySqlTypeInfo {
                <str as sqlx::Type<sqlx::MySql>>::type_info()
            }

            fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
                <str as sqlx::Type<sqlx::MySql>>::compatible(ty)
            }
        }

        impl<'de> sqlx::Decode<'de, sqlx::MySql> for $name {
            fn decode(
                value: sqlx::mysql::MySqlValueRef<'de>,
            ) -> Result<Self, sqlx::error::BoxDynError> {
                let literal = <&str as sqlx::Decode<sqlx::MySql>>::decode(value)?;
                Ok(Self::parse(literal)?)
            }
        }

        impl sqlx::Encode<'_, sqlx::MySql> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut Vec<u8>,
            ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                <&str as sqlx::Encode<sqlx::MySql>>::encode(self.as_str(), buf)
            }
        }
    };
}

/// 兼容路径：crate 内部既有 `use crate::wordlist::wordlist_enum` 引用
/// 不变；宏本体已 #[macro_export] 至 crate 根（跨 crate 用
/// `chargepilot_domain::wordlist_enum!`）。
pub use wordlist_enum;
