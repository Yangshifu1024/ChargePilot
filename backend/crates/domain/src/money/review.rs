//! 人工复核共用规则：依据校验与重放比对。

/// 复核重放与首次记录不一致。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("复核重放与首次记录不一致")]
pub struct ReviewConflict;

/// 复核依据的字数上限。
pub const MAX_REVIEW_COMMENT: usize = 255;

/// 校验人工复核依据：非空、不超过 255 字，且不含控制字符。
pub fn valid_review_comment(comment: &str) -> bool {
    if comment.trim().is_empty() || comment.chars().count() > MAX_REVIEW_COMMENT {
        return false;
    }
    !comment.chars().any(char::is_control)
}

/// 某次人工复核的首次记录。
#[derive(Debug, Clone)]
pub struct PriorReview {
    pub actor_id: u64,
    pub approved: bool,
    pub comment: String,
}

/// 比对审核人、结论与依据；全部一致才沿用首次回执。
pub fn review_replay(
    prior: &PriorReview,
    actor_id: u64,
    approved: bool,
    comment: &str,
) -> Result<(), ReviewConflict> {
    if prior.actor_id != actor_id || prior.approved != approved || prior.comment != comment {
        return Err(ReviewConflict);
    }
    Ok(())
}

/// 比对解冻操作的审核人与依据；解冻没有通过或驳回结论。
pub fn release_replay(
    prior: &PriorReview,
    actor_id: u64,
    comment: &str,
) -> Result<(), ReviewConflict> {
    if prior.actor_id != actor_id || prior.comment != comment {
        return Err(ReviewConflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_comment_validation() {
        assert!(valid_review_comment("正常依据"));
        assert!(!valid_review_comment("   "));
        assert!(!valid_review_comment(&"长".repeat(256)));
        assert!(!valid_review_comment("含控制\u{7}字符"));
    }

    #[test]
    fn replay_rules() {
        let prior = PriorReview {
            actor_id: 1,
            approved: true,
            comment: "ok".into(),
        };
        assert!(review_replay(&prior, 1, true, "ok").is_ok());
        assert!(review_replay(&prior, 2, true, "ok").is_err());
        assert!(release_replay(&prior, 1, "ok").is_ok());
        assert!(release_replay(&prior, 1, "changed").is_err());
    }
}
