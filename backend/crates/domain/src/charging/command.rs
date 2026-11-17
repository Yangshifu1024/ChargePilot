//! 充电命令域词表（词表补齐）。
//! 命令通道与停止命令两条词表同形不同表，不得互换。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// charge_command.status 列的启动命令状态（五值）。
    ChargeCommandStatus: {
        Pending = "pending",
        Sent = "sent",
        Acked = "acked",
        Stopping = "stopping",
        Rejected = "rejected",
    }
);

wordlist_enum!(
    /// charge_stop_command.status 列的停止命令状态（四值；无 stopping）。
    ChargeStopCommandStatus: {
        Pending = "pending",
        Sent = "sent",
        Acked = "acked",
        Rejected = "rejected",
    }
);

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let cmd = [
            (ChargeCommandStatus::Pending, "pending"),
            (ChargeCommandStatus::Sent, "sent"),
            (ChargeCommandStatus::Acked, "acked"),
            (ChargeCommandStatus::Stopping, "stopping"),
            (ChargeCommandStatus::Rejected, "rejected"),
        ];
        for (v, s) in cmd {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let stop = [
            (ChargeStopCommandStatus::Pending, "pending"),
            (ChargeStopCommandStatus::Sent, "sent"),
            (ChargeStopCommandStatus::Acked, "acked"),
            (ChargeStopCommandStatus::Rejected, "rejected"),
        ];
        for (v, s) in stop {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
