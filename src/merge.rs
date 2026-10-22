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

/// `None` means `statusCheckRollup` / `gh pr checks` listed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksVerdict {
    None,
    AllGreen,
    Pending,
    Failed,
}

/// GitHub `mergeStateStatus`. Only `CLEAN` is merge-ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    Clean,
    Behind,
    Blocked,
    Unstable,
    Dirty,
    Draft,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeInput {
    pub life: PrLife,
    pub draft: bool,
    pub mergeable: Mergeability,
    pub merge_state: MergeState,
    pub checks: ChecksVerdict,
    pub approvals: u32,
    pub changes_requested: bool,
}

pub fn parse_merge_state(value: &str) -> MergeState {
    match value.to_ascii_uppercase().as_str() {
        "CLEAN" => MergeState::Clean,
        "BEHIND" => MergeState::Behind,
        "BLOCKED" => MergeState::Blocked,
        "UNSTABLE" => MergeState::Unstable,
        "DIRTY" => MergeState::Dirty,
        "DRAFT" => MergeState::Draft,
        _ => MergeState::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    MergeReady,
    Merged,
    Closed,
    ReviewApproved,
}

impl StopReason {
    pub fn message(self, lang: crate::lang::Lang, number: u64) -> String {
        match self {
            Self::MergeReady => lang.say(
                &format!(
                    "PR #{number} はマージ可能です（CLEAN、チェックは空かすべて成功、未解決の changes requested なし、コンフリクトなし）。終了します。"
                ),
                &format!(
                    "PR #{number} is merge-ready (CLEAN, no checks or all checks passed, no outstanding changes requested, mergeable). Exiting."
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
            Self::ReviewApproved => lang.say(
                "承認相当のレビューを投稿したので終了します",
                "Posted an approve-equivalent review. Exiting.",
            ),
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::MergeReady | Self::Merged | Self::ReviewApproved => 0,
            Self::Closed => 2,
        }
    }
}

/// Review mode does not stop just because the PR is merge-ready.
pub fn review_life_stop(input: &MergeInput) -> Option<StopReason> {
    match input.life {
        PrLife::Merged => Some(StopReason::Merged),
        PrLife::Closed => Some(StopReason::Closed),
        PrLife::Open => None,
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

/// Code-mode merge-ready: open, not a draft, `mergeStateStatus` CLEAN,
/// not conflicting, and no outstanding changes-requested review.
/// A non-empty check list must be all green. Pending waits. An empty list
/// means this PR has no CI, so CI is ignored. Approvals are not required.
pub fn is_merge_ready(input: &MergeInput) -> bool {
    input.life == PrLife::Open
        && !input.draft
        && input.merge_state == MergeState::Clean
        && input.mergeable != Mergeability::Conflicting
        && checks_allow_merge(input.checks)
        && !input.changes_requested
}

fn checks_allow_merge(checks: ChecksVerdict) -> bool {
    matches!(checks, ChecksVerdict::None | ChecksVerdict::AllGreen)
}

/// One line per code poll. Empty CI is `none(ok)`; pending is `pending(wait)`.
pub fn merge_ready_line(input: &MergeInput) -> String {
    let ci = match input.checks {
        ChecksVerdict::None => "none(ok)",
        ChecksVerdict::AllGreen => "green(ok)",
        ChecksVerdict::Pending => "pending(wait)",
        ChecksVerdict::Failed => "failed(block)",
    };
    let state = match input.merge_state {
        MergeState::Clean => "CLEAN",
        MergeState::Behind => "BEHIND",
        MergeState::Blocked => "BLOCKED",
        MergeState::Unstable => "UNSTABLE",
        MergeState::Dirty => "DIRTY",
        MergeState::Draft => "DRAFT",
        MergeState::Unknown => "UNKNOWN",
    };
    let changes = if input.changes_requested { 1 } else { 0 };
    let answer = if is_merge_ready(input) { "yes" } else { "no" };
    format!("merge-ready? CI {ci} / {state} / changes requested {changes} → {answer}")
}

/// What the merge prompt decided. Enter, Esc, and Ctrl-C are exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAnswer {
    Merge,
    Exit,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKey {
    Yes,
    No,
    Continue,
    Enter,
    Esc,
    CtrlC,
}

pub fn merge_key(key: MergeKey) -> MergeAnswer {
    match key {
        MergeKey::Yes => MergeAnswer::Merge,
        MergeKey::Continue => MergeAnswer::Continue,
        MergeKey::No | MergeKey::Enter | MergeKey::Esc | MergeKey::CtrlC => MergeAnswer::Exit,
    }
}

/// `--merge-yes` merges without asking. `-y` is ignored.
/// A non-TTY without `--merge-yes` exits without merging.
/// `None` means the TUI must ask.
pub fn auto_merge_answer(merge_yes: bool, _yes: bool, tty: bool) -> Option<MergeAnswer> {
    if merge_yes {
        Some(MergeAnswer::Merge)
    } else if !tty {
        Some(MergeAnswer::Exit)
    } else {
        None
    }
}

/// Re-check immediately before `gh pr merge`. A moved head does not merge.
pub fn commit_merge(ready: bool, expected_sha: &str, current_sha: &str) -> bool {
    ready && !expected_sha.is_empty() && expected_sha == current_sha
}

/// Merge is offered only after the same head has been merge-ready twice in a row.
/// The first CLEAN poll after a push, before CI has registered, does not merge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReadyStreak {
    sha: Option<String>,
}

impl MergeReadyStreak {
    pub fn observe(&mut self, sha: &str, ready: bool) -> bool {
        if !ready || sha.is_empty() {
            self.sha = None;
            return false;
        }
        if self.sha.as_deref() == Some(sha) {
            return true;
        }
        self.sha = Some(sha.to_string());
        false
    }
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
            merge_state: MergeState::Clean,
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
        assert_eq!(
            stop_reason(&no_approval),
            Some(StopReason::MergeReady),
            "CLEAN with registered green checks does not also require an approval count"
        );

        let mut changes = open_ready();
        changes.changes_requested = true;
        assert_eq!(stop_reason(&changes), None);

        let mut conflict = open_ready();
        conflict.mergeable = Mergeability::Conflicting;
        assert_eq!(stop_reason(&conflict), None);

        let mut unknown = open_ready();
        unknown.mergeable = Mergeability::Unknown;
        assert_eq!(
            stop_reason(&unknown),
            Some(StopReason::MergeReady),
            "CLEAN and not conflicting is enough when mergeable is unknown"
        );

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
        no_checks.approvals = 0;
        assert!(
            is_merge_ready(&no_checks),
            "an empty check list means no CI and is ignored when CLEAN"
        );
        assert_eq!(
            merge_ready_line(&no_checks),
            "merge-ready? CI none(ok) / CLEAN / changes requested 0 → yes"
        );
        let mut pending_empty_state = no_checks.clone();
        pending_empty_state.checks = ChecksVerdict::Pending;
        assert!(!is_merge_ready(&pending_empty_state));
        assert_eq!(
            merge_ready_line(&pending_empty_state),
            "merge-ready? CI pending(wait) / CLEAN / changes requested 0 → no"
        );

        let mut behind = open_ready();
        behind.merge_state = MergeState::Behind;
        assert!(!is_merge_ready(&behind));
        let mut blocked = open_ready();
        blocked.merge_state = MergeState::Blocked;
        assert!(!is_merge_ready(&blocked));
        let mut unstable = open_ready();
        unstable.merge_state = MergeState::Unstable;
        assert!(!is_merge_ready(&unstable));
    }

    #[test]
    fn merge_prompt_has_three_branches_and_ignores_yes() {
        assert_eq!(merge_key(MergeKey::Yes), MergeAnswer::Merge);
        assert_eq!(merge_key(MergeKey::No), MergeAnswer::Exit);
        assert_eq!(merge_key(MergeKey::Continue), MergeAnswer::Continue);
        assert_eq!(merge_key(MergeKey::Enter), MergeAnswer::Exit);
        assert_eq!(merge_key(MergeKey::Esc), MergeAnswer::Exit);
        assert_eq!(merge_key(MergeKey::CtrlC), MergeAnswer::Exit);

        assert_eq!(auto_merge_answer(false, true, true), None);
        assert_eq!(
            auto_merge_answer(true, false, false),
            Some(MergeAnswer::Merge)
        );
        assert_eq!(
            auto_merge_answer(false, true, false),
            Some(MergeAnswer::Exit)
        );
        assert!(commit_merge(true, "abc", "abc"));
        assert!(!commit_merge(true, "abc", "zzz"));
        assert!(!commit_merge(false, "abc", "abc"));
    }

    #[test]
    fn merge_now_recheck_rejects_a_moved_sha() {
        assert!(
            commit_merge(true, "abc", "abc"),
            "merge_now merges only when the re-fetched head is still the expected SHA"
        );
        assert!(
            !commit_merge(true, "abc", "def"),
            "a moved head SHA must not merge"
        );
        assert!(!commit_merge(false, "abc", "abc"));
        assert!(!commit_merge(true, "", ""));
    }

    #[test]
    fn merge_ready_must_repeat_on_the_same_sha() {
        let mut streak = MergeReadyStreak::default();
        assert!(
            !streak.observe("abc", true),
            "the first CLEAN poll, including empty CI right after a push, does not merge"
        );
        assert!(streak.observe("abc", true));
        assert!(!streak.observe("def", true), "a new head starts over");
        assert!(streak.observe("def", true));
        assert!(!streak.observe("def", false));
        assert!(
            !streak.observe("def", true),
            "a not-ready poll clears the streak"
        );
        assert!(!streak.observe("", true));
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
