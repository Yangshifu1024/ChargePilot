//! 结算域共享词表（票 08）：计费任务、计量复核、发票与人工费用复核。

use crate::wordlist::wordlist_enum;

wordlist_enum!(
    /// charge_billing_job.status 列的计费任务状态（三值）。
    BillingJobStatus: {
        Pending = "pending",
        ManualReview = "manual_review",
        Done = "done",
    }
);

wordlist_enum!(
    /// charge_meter_review.status 列的计量复核状态（三值；双复核准入，
    /// 首复核不落状态列——见 charge_meter_review 表注释）。
    MeterReviewStatus: {
        AwaitingSecond = "awaiting_second",
        Approved = "approved",
        Rejected = "rejected",
    }
);

wordlist_enum!(
    /// invoice_request.invoice_type 列的发票类型（两值）。
    InvoiceType: {
        Normal = "normal",
        VatSpecial = "vat_special",
    }
);

wordlist_enum!(
    /// invoice_request.review_status 列的开票申请审核状态（四值；
    /// issued 表示已开票终结）。
    InvoiceRequestReviewStatus: {
        Pending = "pending",
        Approved = "approved",
        Rejected = "rejected",
        Issued = "issued",
    }
);

wordlist_enum!(
    /// invoice_admin_review.review_status 列的单次后台审核结论（三值；
    /// awaiting_second 为待第二人复核）。
    InvoiceAdminReviewStatus: {
        AwaitingSecond = "awaiting_second",
        Approved = "approved",
        Rejected = "rejected",
    }
);

wordlist_enum!(
    /// manual_fee_review.status 列的人工费用复核状态（两值）。
    ManualFeeReviewStatus: {
        Pending = "pending",
        Resolved = "resolved",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    /// 字面量对照（票 08 硬规则）：serde 输出与 DB ENUM 字面量逐字节一致。
    #[test]
    fn db_literals_match_serde() {
        use serde_json::to_string as json;
        let job = [
            (BillingJobStatus::Pending, "pending"),
            (BillingJobStatus::ManualReview, "manual_review"),
            (BillingJobStatus::Done, "done"),
        ];
        for (v, s) in job {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let meter = [
            (MeterReviewStatus::AwaitingSecond, "awaiting_second"),
            (MeterReviewStatus::Approved, "approved"),
            (MeterReviewStatus::Rejected, "rejected"),
        ];
        for (v, s) in meter {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let inv_type = [
            (InvoiceType::Normal, "normal"),
            (InvoiceType::VatSpecial, "vat_special"),
        ];
        for (v, s) in inv_type {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let req_review = [
            (InvoiceRequestReviewStatus::Pending, "pending"),
            (InvoiceRequestReviewStatus::Approved, "approved"),
            (InvoiceRequestReviewStatus::Rejected, "rejected"),
            (InvoiceRequestReviewStatus::Issued, "issued"),
        ];
        for (v, s) in req_review {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let admin_review = [
            (InvoiceAdminReviewStatus::AwaitingSecond, "awaiting_second"),
            (InvoiceAdminReviewStatus::Approved, "approved"),
            (InvoiceAdminReviewStatus::Rejected, "rejected"),
        ];
        for (v, s) in admin_review {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
        let fee = [
            (ManualFeeReviewStatus::Pending, "pending"),
            (ManualFeeReviewStatus::Resolved, "resolved"),
        ];
        for (v, s) in fee {
            assert_eq!(v.as_str(), s);
            assert_eq!(json(&v).unwrap(), format!("\"{s}\""));
        }
    }
}
