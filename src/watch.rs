use std::collections::BTreeSet;

use crate::merge::{stop_reason, MergeInput, StopReason};

/// A review comment or conversation comment ghost may hand to the coding agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub author: String,
    pub body: String,
    pub kind: String,
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
) -> Result<CodeAction, MaxTurns> {
    if let Some(reason) = stop_reason(&snap.merge) {
        return Ok(CodeAction::Stop(reason));
    }
    let new_notes = unseen_notes(&snap.notes, seen);
    let ci_failed = snap.merge.checks == crate::merge::ChecksVerdict::Failed;
    if !ci_failed && new_notes.is_empty() {
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
        }
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
            decide_code(&pr, &seen, 0, 20).unwrap(),
            CodeAction::Idle,
            "green CI and no comments must not call the agent"
        );

        pr.merge.checks = ChecksVerdict::Pending;
        assert_eq!(decide_code(&pr, &seen, 0, 20).unwrap(), CodeAction::Idle);

        pr.merge.checks = ChecksVerdict::Failed;
        match decide_code(&pr, &seen, 0, 20).unwrap() {
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
        pr.notes.push(Note {
            id: "c1".into(),
            author: "ada".into(),
            body: "please rename this".into(),
            kind: "review".into(),
        });
        match decide_code(&pr, &seen, 0, 20).unwrap() {
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
            decide_code(&pr, &seen, 0, 20).unwrap(),
            CodeAction::Idle,
            "already-seen comments are not a new update"
        );
    }

    #[test]
    fn sha_change_alone_does_not_wake_code_mode() {
        let pr = snap("brand-new", ChecksVerdict::AllGreen);
        assert_eq!(
            decide_code(&pr, &BTreeSet::new(), 0, 20).unwrap(),
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
        let branch = "ghost/issue-7";
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
