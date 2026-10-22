use std::collections::BTreeSet;

use clap::ValueEnum;

use crate::merge::{stop_reason, MergeInput, StopReason};

/// A review submission, inline review comment, or issue comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub author: String,
    pub body: String,
    pub kind: String,
    /// GitHub `author_association` (`OWNER`, `MEMBER`, `COLLABORATOR`, …).
    pub association: String,
    /// RFC3339 timestamp from the API, when present.
    pub created_at: String,
    pub viewer_authored: bool,
}

impl Note {
    pub fn new(
        id: impl Into<String>,
        author: impl Into<String>,
        body: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            author: author.into(),
            body: body.into(),
            kind: kind.into(),
            association: String::new(),
            created_at: String::new(),
            viewer_authored: false,
        }
    }
}

/// Raw counts from the three feedback endpoints, before filtering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedbackCounts {
    pub reviews: usize,
    pub inline: usize,
    pub comments: usize,
}

pub fn feedback_log(counts: FeedbackCounts) -> String {
    format!(
        "reviews {}, inline {}, comments {}",
        counts.reviews, counts.inline, counts.comments
    )
}

/// Who may wake code mode besides the PR author and the repo owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum CommentTrust {
    /// `OWNER`, `MEMBER`, and `COLLABORATOR`.
    #[default]
    Collaborators,
    /// Collaborators plus `CONTRIBUTOR`.
    Contributors,
    /// Every author association.
    All,
}

impl std::fmt::Display for CommentTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Collaborators => "collaborators",
            Self::Contributors => "contributors",
            Self::All => "all",
        })
    }
}

/// CI failure ghost has already handed to the agent for this SHA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiHandled {
    pub sha: String,
    pub checks: BTreeSet<String>,
}

impl CiHandled {
    pub fn new(sha: impl Into<String>, checks: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            sha: sha.into(),
            checks: checks.into_iter().map(|c| c.as_ref().to_string()).collect(),
        }
    }

    pub fn covers(&self, sha: &str, checks: &[String]) -> bool {
        self.sha == sha && self.checks == checks.iter().cloned().collect()
    }
}

/// Snapshot ghost polls from `gh pr view`.
#[derive(Debug, Clone)]
pub struct PrSnap {
    pub number: u64,
    pub title: String,
    pub life: crate::merge::PrLife,
    pub draft: bool,
    pub head_sha: String,
    pub head_branch: String,
    pub base_branch: String,
    pub merge: MergeInput,
    pub failing_checks: Vec<String>,
    pub notes: Vec<Note>,
    pub ci_label: String,
    pub author: String,
    pub created_at: String,
    pub feedback: FeedbackCounts,
    pub feedback_warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTurns;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    Stop(StopReason),
    /// Head SHA matches the last reviewed SHA. Do not call the agent.
    Idle,
    Review {
        full_diff: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeAction {
    Stop(StopReason),
    /// CI is not failing and there are no unseen review comments.
    Idle,
    Fix {
        ci_failed: bool,
        new_notes: Vec<Note>,
    },
}

/// Review mode calls the agent only when the head SHA changed.
/// The same SHA is a no-op even if comments arrived. Merge-ready wins over a new SHA.
pub fn decide_review(
    snap: &PrSnap,
    last_reviewed_sha: Option<&str>,
    turns: u32,
    max_turns: u32,
) -> Result<ReviewAction, MaxTurns> {
    if let Some(reason) = stop_reason(&snap.merge) {
        return Ok(ReviewAction::Stop(reason));
    }
    if !review_needed(last_reviewed_sha, &snap.head_sha) {
        return Ok(ReviewAction::Idle);
    }
    if turns >= max_turns {
        return Err(MaxTurns);
    }
    let full_diff = !matches!(last_reviewed_sha, Some(prev) if !prev.is_empty());
    Ok(ReviewAction::Review { full_diff })
}

pub fn review_needed(last_reviewed_sha: Option<&str>, head_sha: &str) -> bool {
    match last_reviewed_sha {
        Some(prev) if !prev.is_empty() && prev == head_sha => false,
        _ => true,
    }
}

pub fn decide_code(
    snap: &PrSnap,
    seen: &BTreeSet<String>,
    turns: u32,
    max_turns: u32,
    handled_ci: Option<&CiHandled>,
) -> Result<CodeAction, MaxTurns> {
    if let Some(reason) = stop_reason(&snap.merge) {
        return Ok(CodeAction::Stop(reason));
    }
    let new_notes = unseen_notes(&snap.notes, seen);
    let ci_failed = snap.merge.checks == crate::merge::ChecksVerdict::Failed;
    let ci_is_new = ci_failed
        && !handled_ci
            .map(|h| h.covers(&snap.head_sha, &snap.failing_checks))
            .unwrap_or(false);
    if !ci_is_new && new_notes.is_empty() {
        return Ok(CodeAction::Idle);
    }
    if turns >= max_turns {
        return Err(MaxTurns);
    }
    Ok(CodeAction::Fix {
        ci_failed,
        new_notes,
    })
}

pub fn wakes_agent_review(action: &ReviewAction) -> bool {
    matches!(action, ReviewAction::Review { .. })
}

pub fn wakes_agent_code(action: &CodeAction) -> bool {
    matches!(action, CodeAction::Fix { .. })
}

pub fn association_trusted(association: &str, trust: CommentTrust) -> bool {
    let a = association.to_ascii_uppercase();
    match trust {
        CommentTrust::All => true,
        CommentTrust::Contributors => {
            matches!(
                a.as_str(),
                "OWNER" | "MEMBER" | "COLLABORATOR" | "CONTRIBUTOR"
            )
        }
        CommentTrust::Collaborators => matches!(a.as_str(), "OWNER" | "MEMBER" | "COLLABORATOR"),
    }
}

pub fn author_is(note: &Note, login: &str) -> bool {
    !login.is_empty() && note.author.eq_ignore_ascii_case(login)
}

pub fn mark_viewer(notes: &mut [Note], login: &str) {
    for note in notes {
        note.viewer_authored = author_is(note, login);
    }
}

/// True when this note is the agent's own reply, not a human review from the same account.
pub fn is_self_generated(note: &Note, login: &str, agent_ids: &BTreeSet<String>) -> bool {
    if !agent_ids.contains(&note.id) {
        return false;
    }
    if login.is_empty() {
        return note.viewer_authored;
    }
    author_is(note, login)
}

/// PR author and repository owner are always trusted. Others follow `trust`.
pub fn note_trusted(note: &Note, trust: CommentTrust, pr_author: &str, repo_owner: &str) -> bool {
    if !pr_author.is_empty() && note.author.eq_ignore_ascii_case(pr_author) {
        return true;
    }
    if !repo_owner.is_empty() && note.author.eq_ignore_ascii_case(repo_owner) {
        return true;
    }
    association_trusted(&note.association, trust)
}

pub fn actionable_notes(
    notes: &[Note],
    trust: CommentTrust,
    login: &str,
    pr_author: &str,
    repo_owner: &str,
    agent_ids: &BTreeSet<String>,
) -> Vec<Note> {
    notes
        .iter()
        .filter(|n| {
            !n.body.trim().is_empty()
                && !is_self_generated(n, login, agent_ids)
                && note_trusted(n, trust, pr_author, repo_owner)
        })
        .cloned()
        .collect()
}

/// Ids to treat as the agent's own at the issue → code handoff.
/// Comments created after the PR was opened are not included, so a review
/// posted on the new PR is still unseen. Unknown timestamps are not baselined.
pub fn baseline_agent_comments(
    notes: &[Note],
    login: &str,
    pr_created_at: Option<&str>,
) -> BTreeSet<String> {
    notes
        .iter()
        .filter(|n| author_is(n, login) && !created_after(&n.created_at, pr_created_at))
        .map(|n| n.id.clone())
        .collect()
}

fn created_after(created: &str, pr_created_at: Option<&str>) -> bool {
    match pr_created_at.map(str::trim).filter(|s| !s.is_empty()) {
        Some(pr) if !created.trim().is_empty() => created.trim() > pr,
        _ => true,
    }
}

/// Append `Closes #N` when the body does not already close the issue.
pub fn ensure_closing_line(body: &str, issue: u64) -> String {
    if body_closes_issue(body, issue) {
        return body.to_string();
    }
    let line = format!("Closes #{issue}");
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        format!("{line}\n")
    } else {
        format!("{trimmed}\n\n{line}\n")
    }
}

pub fn unseen_notes(notes: &[Note], seen: &BTreeSet<String>) -> Vec<Note> {
    notes
        .iter()
        .filter(|n| !n.id.is_empty() && !seen.contains(&n.id))
        .cloned()
        .collect()
}

/// True when a PR body or a linked-issue list refers to `issue`.
pub fn body_closes_issue(body: &str, issue: u64) -> bool {
    let lower = body.to_ascii_lowercase();
    let needle = format!("#{issue}");
    let mut start = 0;
    while let Some(rel) = lower[start..].find(&needle) {
        let abs = start + rel;
        let after = lower[abs + needle.len()..]
            .chars()
            .next()
            .map(|c| !c.is_ascii_digit())
            .unwrap_or(true);
        if after && keyword_before(&lower[..abs]) {
            return true;
        }
        start = abs + needle.len();
    }
    false
}

fn keyword_before(before: &str) -> bool {
    let trimmed = before.trim_end();
    const KEYS: &[&str] = &[
        "closes", "close", "closed", "fixes", "fix", "fixed", "resolves", "resolve", "resolved",
    ];
    KEYS.iter().any(|kw| ends_with_keyword(trimmed, kw))
}

fn ends_with_keyword(s: &str, kw: &str) -> bool {
    match s.strip_suffix(kw) {
        Some(rest) => rest
            .chars()
            .last()
            .map(|c| !c.is_ascii_alphanumeric())
            .unwrap_or(true),
        None => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCandidate {
    pub number: u64,
    pub state: String,
    pub head_ref: String,
    pub body: String,
    pub closing_numbers: Vec<u64>,
}

/// Pick an existing PR for an issue so ghost does not open a duplicate.
pub fn find_issue_pr<'a>(
    candidates: &'a [PrCandidate],
    issue: u64,
    branch: &str,
) -> Option<&'a PrCandidate> {
    let mut best: Option<&PrCandidate> = None;
    for cand in candidates {
        let branch_match = cand.head_ref == branch;
        let link_match =
            cand.closing_numbers.contains(&issue) || body_closes_issue(&cand.body, issue);
        if !(branch_match || link_match) {
            continue;
        }
        best = Some(match best {
            None => cand,
            Some(prev) => prefer_pr(prev, cand, branch),
        });
    }
    best
}

fn prefer_pr<'a>(a: &'a PrCandidate, b: &'a PrCandidate, branch: &str) -> &'a PrCandidate {
    let rank = |c: &PrCandidate| {
        let open = if c.state.eq_ignore_ascii_case("OPEN") {
            2
        } else if c.state.eq_ignore_ascii_case("MERGED") {
            1
        } else {
            0
        };
        let on_branch = if c.head_ref == branch { 1 } else { 0 };
        (open, on_branch, c.number)
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::{ChecksVerdict, MergeInput, Mergeability, PrLife};

    fn snap(sha: &str, checks: ChecksVerdict) -> PrSnap {
        PrSnap {
            number: 3,
            title: "t".into(),
            life: PrLife::Open,
            draft: false,
            head_sha: sha.into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
            merge: MergeInput {
                life: PrLife::Open,
                draft: false,
                mergeable: Mergeability::Mergeable,
                checks,
                approvals: 0,
                changes_requested: false,
            },
            failing_checks: vec![],
            notes: vec![],
            ci_label: "pending".into(),
            author: "cympfh".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            feedback: FeedbackCounts::default(),
            feedback_warnings: vec![],
        }
    }

    fn code(pr: &PrSnap, seen: &BTreeSet<String>) -> CodeAction {
        decide_code(pr, seen, 0, 20, None).unwrap()
    }

    #[test]
    fn skips_review_when_sha_unchanged() {
        assert!(!review_needed(Some("abc"), "abc"));
        assert!(review_needed(None, "abc"));
        assert!(review_needed(Some("abc"), "def"));
        assert!(review_needed(Some(""), "abc"));

        let pr = snap("abc", ChecksVerdict::Pending);
        assert_eq!(
            decide_review(&pr, Some("abc"), 0, 20).unwrap(),
            ReviewAction::Idle
        );
        assert_eq!(
            decide_review(&pr, None, 0, 20).unwrap(),
            ReviewAction::Review { full_diff: true }
        );
        assert_eq!(
            decide_review(&pr, Some("old"), 0, 20).unwrap(),
            ReviewAction::Review { full_diff: false }
        );
    }

    #[test]
    fn review_stops_when_merge_ready_even_if_sha_changed() {
        let mut pr = snap("new", ChecksVerdict::AllGreen);
        pr.merge.approvals = 1;
        assert_eq!(
            decide_review(&pr, Some("old"), 0, 20).unwrap(),
            ReviewAction::Stop(StopReason::MergeReady)
        );
    }

    #[test]
    fn review_does_not_call_agent_at_max_turns() {
        let pr = snap("new", ChecksVerdict::Pending);
        assert!(decide_review(&pr, Some("old"), 20, 20).is_err());
        assert!(decide_review(&pr, Some("old"), 3, 3).is_err());
    }

    #[test]
    fn code_acts_only_on_failure_or_new_notes() {
        let mut pr = snap("abc", ChecksVerdict::AllGreen);
        let seen = BTreeSet::new();
        assert_eq!(
            code(&pr, &seen),
            CodeAction::Idle,
            "green CI and no comments must not call the agent"
        );

        pr.merge.checks = ChecksVerdict::Pending;
        assert_eq!(code(&pr, &seen), CodeAction::Idle);

        pr.merge.checks = ChecksVerdict::Failed;
        match decide_code(&pr, &seen, 0, 20, None).unwrap() {
            CodeAction::Fix {
                ci_failed,
                new_notes,
            } => {
                assert!(ci_failed);
                assert!(new_notes.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }

        pr.merge.checks = ChecksVerdict::AllGreen;
        pr.notes
            .push(Note::new("c1", "ada", "please rename this", "review"));
        match decide_code(&pr, &seen, 0, 20, None).unwrap() {
            CodeAction::Fix {
                ci_failed,
                new_notes,
            } => {
                assert!(!ci_failed);
                assert_eq!(new_notes.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }

        let mut seen = BTreeSet::new();
        seen.insert("c1".to_string());
        assert_eq!(
            decide_code(&pr, &seen, 0, 20, None).unwrap(),
            CodeAction::Idle,
            "already-seen comments are not a new update"
        );
    }

    #[test]
    fn persistent_ci_failure_does_not_retrigger_until_the_set_changes() {
        let mut pr = snap("abc", ChecksVerdict::Failed);
        pr.failing_checks = vec!["build".into()];
        pr.ci_label = "failed".into();
        let seen = BTreeSet::new();
        let handled = CiHandled::new("abc", ["build"]);
        assert_eq!(
            decide_code(&pr, &seen, 0, 20, Some(&handled)).unwrap(),
            CodeAction::Idle
        );
        pr.failing_checks = vec!["lint".into(), "build".into()];
        assert!(matches!(
            decide_code(&pr, &seen, 0, 20, Some(&handled)).unwrap(),
            CodeAction::Fix {
                ci_failed: true,
                ..
            }
        ));
        pr.failing_checks = vec!["build".into()];
        pr.head_sha = "def".into();
        assert!(matches!(
            decide_code(&pr, &seen, 0, 20, Some(&handled)).unwrap(),
            CodeAction::Fix {
                ci_failed: true,
                ..
            }
        ));
    }

    #[test]
    fn quiet_poll_does_not_wake_the_agent() {
        let pr = snap("abc", ChecksVerdict::None);
        let review = decide_review(&pr, Some("abc"), 0, 20).unwrap();
        assert!(!wakes_agent_review(&review));
        let code_action = decide_code(&pr, &BTreeSet::new(), 0, 20, None).unwrap();
        assert!(!wakes_agent_code(&code_action));
        assert_eq!(code_action, CodeAction::Idle);
    }

    #[test]
    fn issue_to_code_review_comment_is_not_idle() {
        // issue/N opened PR #2, then the repo owner (also the gh user) left a review.
        let mut pr = snap("abc", ChecksVerdict::None);
        pr.number = 2;
        pr.ci_label = "none".into();
        pr.author = "cympfh".into();
        pr.created_at = "2024-06-01T00:00:00Z".into();
        pr.feedback = FeedbackCounts {
            reviews: 1,
            inline: 2,
            comments: 0,
        };
        assert_eq!(feedback_log(pr.feedback), "reviews 1, inline 2, comments 0");
        pr.notes.push(Note {
            id: "review:10".into(),
            author: "cympfh".into(),
            body: "この分岐を直して".into(),
            kind: "review COMMENTED".into(),
            association: "OWNER".into(),
            created_at: "2024-06-01T00:05:00Z".into(),
            viewer_authored: true,
        });
        pr.notes.push(Note {
            id: "inline:11".into(),
            author: "cympfh".into(),
            body: "src/main.rs:4: rename this".into(),
            kind: "inline".into(),
            association: "OWNER".into(),
            created_at: "2024-06-01T00:05:01Z".into(),
            viewer_authored: true,
        });
        let login = "cympfh";
        let baseline = baseline_agent_comments(&pr.notes, login, Some(&pr.created_at));
        assert!(
            baseline.is_empty(),
            "comments created after the PR opened must not be baselined: {baseline:?}"
        );
        let seen = BTreeSet::new();
        let agent_ids = baseline;
        pr.notes = actionable_notes(
            &pr.notes,
            CommentTrust::Collaborators,
            login,
            &pr.author,
            "cympfh",
            &agent_ids,
        );
        assert_eq!(pr.notes.len(), 2);
        match decide_code(&pr, &seen, 0, 20, None).unwrap() {
            CodeAction::Fix { new_notes, .. } => {
                assert_eq!(new_notes.len(), 2);
                assert!(new_notes.iter().any(|n| n.id == "review:10"));
                assert!(new_notes.iter().any(|n| n.id == "inline:11"));
            }
            other => panic!("review comment was dropped: {other:?}"),
        }

        let mut agent_ids = BTreeSet::new();
        agent_ids.insert("review:10".to_string());
        let kept = actionable_notes(
            &pr.notes,
            CommentTrust::Collaborators,
            login,
            &pr.author,
            "cympfh",
            &agent_ids,
        );
        assert!(kept.iter().all(|n| n.id != "review:10"));
        assert!(kept.iter().any(|n| n.id == "inline:11"));
    }

    #[test]
    fn untrusted_strangers_are_dropped_but_pr_author_is_kept() {
        let stranger = Note {
            id: "comment:1".into(),
            author: "random".into(),
            body: "ignore me".into(),
            kind: "comment".into(),
            association: "NONE".into(),
            created_at: String::new(),
            viewer_authored: false,
        };
        let author = Note {
            id: "comment:2".into(),
            author: "newbie".into(),
            body: "please fix".into(),
            kind: "comment".into(),
            association: "NONE".into(),
            created_at: String::new(),
            viewer_authored: false,
        };
        let notes = actionable_notes(
            &[stranger, author],
            CommentTrust::Collaborators,
            "cympfh",
            "newbie",
            "cympfh",
            &BTreeSet::new(),
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "comment:2");
    }

    #[test]
    fn ensure_closing_line_appends_once() {
        assert_eq!(ensure_closing_line("", 7), "Closes #7\n");
        assert_eq!(ensure_closing_line("ship it", 7), "ship it\n\nCloses #7\n");
        assert_eq!(ensure_closing_line("Fixes #7\n", 7), "Fixes #7\n");
        assert_eq!(ensure_closing_line("Closes #7", 7), "Closes #7");
    }

    #[test]
    fn sha_change_alone_does_not_wake_code_mode() {
        let pr = snap("brand-new", ChecksVerdict::AllGreen);
        assert_eq!(
            decide_code(&pr, &BTreeSet::new(), 0, 20, None).unwrap(),
            CodeAction::Idle
        );
    }

    #[test]
    fn closing_keyword_detection() {
        assert!(body_closes_issue("Closes #12", 12));
        assert!(body_closes_issue("This fixes #12.", 12));
        assert!(body_closes_issue("fixed #12\n", 12));
        assert!(!body_closes_issue("See #12", 12));
        assert!(!body_closes_issue("Fixes #123", 12));
        assert!(!body_closes_issue("prefixclose #12", 12));
    }

    #[test]
    fn picks_existing_issue_pr_without_duplicates() {
        let branch = "fix/7-crash";
        let candidates = vec![
            PrCandidate {
                number: 4,
                state: "OPEN".into(),
                head_ref: "other".into(),
                body: "See #7".into(),
                closing_numbers: vec![],
            },
            PrCandidate {
                number: 9,
                state: "CLOSED".into(),
                head_ref: branch.into(),
                body: "".into(),
                closing_numbers: vec![],
            },
            PrCandidate {
                number: 8,
                state: "OPEN".into(),
                head_ref: "topic".into(),
                body: "Closes #7".into(),
                closing_numbers: vec![7],
            },
        ];
        let found = find_issue_pr(&candidates, 7, branch).unwrap();
        assert_eq!(found.number, 8);
        assert!(find_issue_pr(&candidates[..1], 7, branch).is_none());
    }
}
