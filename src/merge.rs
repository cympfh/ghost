use serde_json::Value;

/// Open, closed without merge, or merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLife {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    Unknown,
}

/// `None` means the PR has no checks (treated as green for merge-ready).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksVerdict {
    None,
    AllGreen,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeInput {
    pub life: PrLife,
    pub draft: bool,
    pub mergeable: Mergeability,
    pub checks: ChecksVerdict,
    pub approvals: u32,
    pub changes_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MergeReady,
    Merged,
    Closed,
}

impl StopReason {
    pub fn message(self, lang: crate::lang::Lang, number: u64) -> String {
        match self {
            Self::MergeReady => lang.say(
                &format!(
                    "PR #{number} はマージ可能です（CI 成功、approve 済み、未解決の changes requested なし、コンフリクトなし）。終了します。"
                ),
                &format!(
                    "PR #{number} is merge-ready (CI green, approved, no outstanding changes requested, mergeable). Exiting."
                ),
            ),
            Self::Merged => lang.say(
                &format!("PR #{number} はマージ済みです。終了します。"),
                &format!("PR #{number} is merged. Exiting."),
            ),
            Self::Closed => lang.say(
                &format!("PR #{number} はクローズされています。終了します。"),
                &format!("PR #{number} is closed. Exiting."),
            ),
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::MergeReady | Self::Merged => 0,
            Self::Closed => 2,
        }
    }
}

pub fn parse_life(state: &str) -> PrLife {
    match state.to_ascii_uppercase().as_str() {
        "MERGED" => PrLife::Merged,
        "CLOSED" => PrLife::Closed,
        _ => PrLife::Open,
    }
}

pub fn parse_mergeable(value: &str) -> Mergeability {
    match value.to_ascii_uppercase().as_str() {
        "MERGEABLE" => Mergeability::Mergeable,
        "CONFLICTING" => Mergeability::Conflicting,
        _ => Mergeability::Unknown,
    }
}

/// Merge-ready: open, not draft, mergeable, every check green (or no checks),
/// at least one approval, and no outstanding changes-requested review.
pub fn is_merge_ready(input: &MergeInput) -> bool {
    input.life == PrLife::Open
        && !input.draft
        && input.mergeable == Mergeability::Mergeable
        && matches!(input.checks, ChecksVerdict::AllGreen | ChecksVerdict::None)
        && input.approvals >= 1
        && !input.changes_requested
}

pub fn stop_reason(input: &MergeInput) -> Option<StopReason> {
    match input.life {
        PrLife::Merged => Some(StopReason::Merged),
        PrLife::Closed => Some(StopReason::Closed),
        PrLife::Open if is_merge_ready(input) => Some(StopReason::MergeReady),
        PrLife::Open => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
    Pass,
    Pending,
    Fail,
}

pub fn judge_checks(rollup: &[Value]) -> (ChecksVerdict, Vec<String>) {
    if rollup.is_empty() {
        return (ChecksVerdict::None, Vec::new());
    }
    let mut failed = Vec::new();
    let mut pending = false;
    for item in rollup {
        let name = check_name(item);
        match classify_check(item) {
            CheckState::Pass => {}
            CheckState::Pending => pending = true,
            CheckState::Fail => failed.push(name),
        }
    }
    if !failed.is_empty() {
        (ChecksVerdict::Failed, failed)
    } else if pending {
        (ChecksVerdict::Pending, Vec::new())
    } else {
        (ChecksVerdict::AllGreen, Vec::new())
    }
}

fn check_name(v: &Value) -> String {
    for key in ["name", "context", "workflowName"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "check".to_string()
}

fn classify_check(v: &Value) -> CheckState {
    let typename = v
        .get("__typename")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if typename == "statuscontext" || v.get("context").is_some() && v.get("state").is_some() {
        return match v
            .get("state")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_ascii_uppercase()
            .as_str()
        {
            "SUCCESS" | "SUCCEEDED" => CheckState::Pass,
            "PENDING" | "EXPECTED" => CheckState::Pending,
            "" => CheckState::Pending,
            _ => CheckState::Fail,
        };
    }
    let status = v
        .get("status")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    if status != "COMPLETED" && !status.is_empty() {
        return CheckState::Pending;
    }
    if status.is_empty() && v.get("conclusion").is_none() {
        return CheckState::Pending;
    }
    match v
        .get("conclusion")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_ascii_uppercase()
        .as_str()
    {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckState::Pass,
        "" => CheckState::Pending,
        _ => CheckState::Fail,
    }
}

pub fn ci_word(verdict: ChecksVerdict) -> &'static str {
    match verdict {
        ChecksVerdict::AllGreen => "green",
        ChecksVerdict::Failed => "failed",
        ChecksVerdict::Pending => "pending",
        ChecksVerdict::None => "none",
    }
}

pub fn ci_badge(verdict: ChecksVerdict) -> &'static str {
    match verdict {
        ChecksVerdict::AllGreen => "CI ✓",
        ChecksVerdict::Failed => "CI ✗",
        ChecksVerdict::Pending => "CI …",
        ChecksVerdict::None => "CI –",
    }
}

#[derive(Debug, Clone)]
pub struct ReviewRef {
    pub author: String,
    pub state: String,
    pub submitted_at: String,
}

/// `(approvals, outstanding changes-requested)`.
///
/// `reviewDecision` from `gh` wins when it is set, because GitHub has already
/// folded dismissed reviews into that value.
pub fn judge_reviews(review_decision: &str, reviews: &[ReviewRef]) -> (u32, bool) {
    let (count, derived_changes) = derive_reviews(reviews);
    match review_decision.to_ascii_uppercase().as_str() {
        "APPROVED" => (count.max(1), false),
        "CHANGES_REQUESTED" => (count, true),
        "REVIEW_REQUIRED" => (0, false),
        _ => (count, derived_changes),
    }
}

fn derive_reviews(reviews: &[ReviewRef]) -> (u32, bool) {
    let mut latest: Vec<&ReviewRef> = Vec::new();
    let mut ordered: Vec<&ReviewRef> = reviews.iter().collect();
    ordered.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
    for review in ordered {
        if review.author.is_empty() {
            continue;
        }
        if let Some(slot) = latest.iter_mut().find(|r| r.author == review.author) {
            // A later COMMENTED review does not erase APPROVED or CHANGES_REQUESTED.
            let comment = review.state.eq_ignore_ascii_case("COMMENTED");
            let substantive = slot.state.eq_ignore_ascii_case("APPROVED")
                || slot.state.eq_ignore_ascii_case("CHANGES_REQUESTED");
            if !(comment && substantive) {
                *slot = review;
            }
        } else {
            latest.push(review);
        }
    }
    let mut approvals = 0u32;
    let mut changes = false;
    for review in latest {
        match review.state.to_ascii_uppercase().as_str() {
            "APPROVED" => approvals += 1,
            "CHANGES_REQUESTED" => changes = true,
            _ => {}
        }
    }
    (approvals, changes)
}

pub fn review_badge(decision: &str, changes_requested: bool, approvals: u32) -> String {
    match decision.to_ascii_uppercase().as_str() {
        "APPROVED" => "approved".into(),
        "CHANGES_REQUESTED" => "changes".into(),
        "REVIEW_REQUIRED" => "review".into(),
        _ if changes_requested => "changes".into(),
        _ if approvals > 0 => "approved".into(),
        _ => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_ready() -> MergeInput {
        MergeInput {
            life: PrLife::Open,
            draft: false,
            mergeable: Mergeability::Mergeable,
            checks: ChecksVerdict::AllGreen,
            approvals: 1,
            changes_requested: false,
        }
    }

    #[test]
    fn merge_ready_requires_every_signal() {
        assert_eq!(stop_reason(&open_ready()), Some(StopReason::MergeReady));

        let mut no_approval = open_ready();
        no_approval.approvals = 0;
        assert_eq!(stop_reason(&no_approval), None);

        let mut changes = open_ready();
        changes.changes_requested = true;
        assert_eq!(stop_reason(&changes), None);

        let mut conflict = open_ready();
        conflict.mergeable = Mergeability::Conflicting;
        assert_eq!(stop_reason(&conflict), None);

        let mut unknown = open_ready();
        unknown.mergeable = Mergeability::Unknown;
        assert_eq!(stop_reason(&unknown), None);

        let mut pending = open_ready();
        pending.checks = ChecksVerdict::Pending;
        assert_eq!(stop_reason(&pending), None);

        let mut failed = open_ready();
        failed.checks = ChecksVerdict::Failed;
        assert_eq!(stop_reason(&failed), None);

        let mut draft = open_ready();
        draft.draft = true;
        assert_eq!(stop_reason(&draft), None);

        let mut no_checks = open_ready();
        no_checks.checks = ChecksVerdict::None;
        assert!(is_merge_ready(&no_checks));
    }

    #[test]
    fn merged_and_closed_stop_immediately() {
        let mut merged = open_ready();
        merged.life = PrLife::Merged;
        merged.checks = ChecksVerdict::Failed;
        assert_eq!(stop_reason(&merged), Some(StopReason::Merged));

        let mut closed = open_ready();
        closed.life = PrLife::Closed;
        assert_eq!(stop_reason(&closed), Some(StopReason::Closed));
        assert_eq!(StopReason::Closed.exit_code(), 2);
        assert_eq!(StopReason::MergeReady.exit_code(), 0);
    }

    #[test]
    fn classifies_check_rollup() {
        let rollup = vec![
            json!({
                "__typename": "CheckRun",
                "name": "build",
                "status": "COMPLETED",
                "conclusion": "SUCCESS"
            }),
            json!({
                "__typename": "CheckRun",
                "name": "lint",
                "status": "IN_PROGRESS",
                "conclusion": ""
            }),
        ];
        assert_eq!(judge_checks(&rollup).0, ChecksVerdict::Pending);

        let failed = vec![
            json!({"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE"}),
            json!({"__typename": "StatusContext", "context": "ci/circle", "state": "SUCCESS"}),
        ];
        let (verdict, names) = judge_checks(&failed);
        assert_eq!(verdict, ChecksVerdict::Failed);
        assert_eq!(names, vec!["build".to_string()]);

        let green = vec![json!({
            "__typename": "CheckRun",
            "name": "build",
            "status": "COMPLETED",
            "conclusion": "SKIPPED"
        })];
        assert_eq!(judge_checks(&green).0, ChecksVerdict::AllGreen);
        assert_eq!(judge_checks(&[]).0, ChecksVerdict::None);
    }

    #[test]
    fn review_decision_overrides_history() {
        let reviews = vec![
            ReviewRef {
                author: "ada".into(),
                state: "APPROVED".into(),
                submitted_at: "2024-01-01T00:00:00Z".into(),
            },
            ReviewRef {
                author: "ada".into(),
                state: "CHANGES_REQUESTED".into(),
                submitted_at: "2024-02-01T00:00:00Z".into(),
            },
        ];
        assert_eq!(judge_reviews("", &reviews), (0, true));
        assert_eq!(judge_reviews("APPROVED", &reviews), (1, false));
        assert_eq!(judge_reviews("CHANGES_REQUESTED", &reviews), (0, true));
        assert_eq!(judge_reviews("REVIEW_REQUIRED", &[]), (0, false));

        let commented_after = vec![
            ReviewRef {
                author: "ada".into(),
                state: "APPROVED".into(),
                submitted_at: "2024-01-01T00:00:00Z".into(),
            },
            ReviewRef {
                author: "ada".into(),
                state: "COMMENTED".into(),
                submitted_at: "2024-03-01T00:00:00Z".into(),
            },
        ];
        assert_eq!(judge_reviews("", &commented_after), (1, false));
    }
}
