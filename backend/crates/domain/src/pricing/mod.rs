//! 计费域共享词表（对照表 pricing 节收尾；S3 批次拆票遗漏，随收尾补齐）。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// pricing_rule.status 列的计费规则状态（两值）。
    PricingRuleStatus: {
        Active = "active",
        Disabled = "disabled",
    }
);

wordlist_enum!(
    /// pricing_rule.channel 列的计费渠道（三值；default 兜底、
    /// temp 临时活动、card 刷卡专用）。
    PricingChannel: {
        Default = "default",
        Temp = "temp",
        Card = "card",
    }
);

wordlist_enum!(
    /// pricing_template.status 列的计费模板状态（两值）。
    PricingTemplateStatus: {
        Active = "active",
        Disabled = "disabled",
    }
);

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    /// 字面量对照：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let rule = [
            (PricingRuleStatus::Active, "active"),
            (PricingRuleStatus::Disabled, "disabled"),
        ];
        for (v, s) in rule {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let channel = [
            (PricingChannel::Default, "default"),
            (PricingChannel::Temp, "temp"),
            (PricingChannel::Card, "card"),
        ];
        for (v, s) in channel {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let template = [
            (PricingTemplateStatus::Active, "active"),
            (PricingTemplateStatus::Disabled, "disabled"),
        ];
        for (v, s) in template {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
