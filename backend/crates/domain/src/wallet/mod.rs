//! 钱包域共享词表：账户、冻结记录、流水方向与业务类型。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// wallet_account.status 列的账户状态（两值；与 identity 的
    /// [`crate::identity::UserStatus`] 分立，取值串相同但不同表不同义）。
    AccountStatus: {
        Active = "active",
        Frozen = "frozen",
    }
);

wordlist_enum!(
    /// risk_freeze_log.status 列的冻结记录状态（两值）。
    /// 与 [`AccountStatus`] 分立：本类型描述「一条冻结记录处于什么阶段」。
    FreezeStatus: {
        Frozen = "frozen",
        Unfrozen = "unfrozen",
    }
);

wordlist_enum!(
    /// wallet_txn.direction 列的资金方向（两值）。
    TxnDirection: {
        In = "in",
        Out = "out",
    }
);

wordlist_enum!(
    /// wallet_txn.biz_type 列的流水业务类型（七值全词表）。
    /// gift/freeze/unfreeze/adjust 当前无写入路径，保留以完整覆盖列约束。
    TxnBizType: {
        Recharge = "recharge",
        Pay = "pay",
        Refund = "refund",
        Gift = "gift",
        Freeze = "freeze",
        Unfreeze = "unfreeze",
        Adjust = "adjust",
    }
);

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let account = [
            (AccountStatus::Active, "active"),
            (AccountStatus::Frozen, "frozen"),
        ];
        for (v, s) in account {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let freeze = [
            (FreezeStatus::Frozen, "frozen"),
            (FreezeStatus::Unfrozen, "unfrozen"),
        ];
        for (v, s) in freeze {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let direction = [(TxnDirection::In, "in"), (TxnDirection::Out, "out")];
        for (v, s) in direction {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let biz = [
            (TxnBizType::Recharge, "recharge"),
            (TxnBizType::Pay, "pay"),
            (TxnBizType::Refund, "refund"),
            (TxnBizType::Gift, "gift"),
            (TxnBizType::Freeze, "freeze"),
            (TxnBizType::Unfreeze, "unfreeze"),
            (TxnBizType::Adjust, "adjust"),
        ];
        for (v, s) in biz {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
