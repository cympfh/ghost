use std::collections::BTreeSet;

use clap::ValueEnum;

use crate::merge::{review_life_stop, stop_reason, MergeInput, StopReason};

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
/// The default is everyone: humans, Copilot, other bots, and outsiders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum CommentTrust {
    /// `OWNER`, `MEMBER`, and `COLLABORATOR`.
    Collaborators,
    /// Collaborators plus `CONTRIBUTOR`.
    Contributors,
    /// Every author: humans, review bots, and outsiders.
    #[default]
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
    /// All three REST comment calls failed. Do not use GraphQL ids this poll.
    pub comments_unavailable: bool,
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

/// Agent's machine-readable review line, for the head SHA it just reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewSignal {
    pub verdict: Option<ReviewVerdict>,
    pub viewer_approved: bool,
}

/// Last `GHOST_VERDICT:` line in the agent output.
pub fn parse_ghost_verdict(text: &str) -> Option<ReviewVerdict> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.split("GHOST_VERDICT:").nth(1) else {
            continue;
        };
        let word = rest
            .split(|c: char| c.is_whitespace() || c == '"' || c == '`' || c == ',')
            .find(|s| !s.is_empty())
            .unwrap_or("");
        found = match word {
            "approve" => Some(ReviewVerdict::Approve),
            "request_changes" => Some(ReviewVerdict::RequestChanges),
            "comment" => Some(ReviewVerdict::Comment),
            _ => found,
        };
    }
    found
}

/// Review stops on merged/closed, or an approve verdict for this head.
/// Merge-ready does not end a review.
pub fn decide_review(
    snap: &PrSnap,
    last_reviewed_sha: Option<&str>,
    turns: u32,
    max_turns: u32,
    signal: ReviewSignal,
) -> Result<ReviewAction, MaxTurns> {
    let _ = signal;
    if let Some(reason) = review_life_stop(&snap.merge) {
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

/// Launch always starts an agent turn. Saved "nothing new" does not skip it.
pub fn opening_runs_agent(saved_says_idle: bool) -> bool {
    let _ = saved_says_idle;
    true
}

/// True when this head should ask the review-exit prompt.
/// An explicit non-approve verdict wins. With no verdict line, a self-review
/// comment that says `LGTM / 承認相当` still counts as approve.
pub fn approve_signal(output: &str) -> bool {
    match parse_ghost_verdict(output) {
        Some(ReviewVerdict::Approve) => true,
        Some(_) => false,
        None => output.contains("LGTM / 承認相当"),
    }
}

/// First turn of a review process always runs, even when this SHA was reviewed before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewLaunch {
    Opening,
    Watching,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewStep {
    Turn,
    Action(ReviewAction),
}

pub fn review_step(launch: ReviewLaunch, opened: bool, action: ReviewAction) -> ReviewStep {
    if launch == ReviewLaunch::Opening && !opened {
        if let ReviewAction::Stop(StopReason::Merged | StopReason::Closed) = action {
            return ReviewStep::Action(action);
        }
        if opening_runs_agent(matches!(action, ReviewAction::Idle)) {
            return ReviewStep::Turn;
        }
    }
    ReviewStep::Action(action)
}

/// Approve (verdict or a real APPROVE on this SHA) asks whether to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterVerdict {
    AskExit,
    Poll,
}

pub fn after_review_verdict(approve: bool) -> AfterVerdict {
    if approve {
        AfterVerdict::AskExit
    } else {
        AfterVerdict::Poll
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewExitAnswer {
    Exit,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewExitKey {
    Yes,
    Continue,
    Enter,
    Esc,
    CtrlC,
}

pub fn review_exit_key(key: ReviewExitKey) -> ReviewExitAnswer {
    match key {
        ReviewExitKey::Continue => ReviewExitAnswer::Continue,
        ReviewExitKey::Yes | ReviewExitKey::Enter | ReviewExitKey::Esc | ReviewExitKey::CtrlC => {
            ReviewExitAnswer::Exit
        }
    }
}

/// `-y` exits without asking. A non-TTY also exits. Review never merges.
pub fn auto_review_exit(yes: bool, tty: bool) -> Option<ReviewExitAnswer> {
    if yes || !tty {
        Some(ReviewExitAnswer::Exit)
    } else {
        None
    }
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

/// Standalone `--code` surveys once before the normal loop. Issue handoff does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLaunch {
    Survey,
    Handoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeStep {
    Survey,
    Action(CodeAction),
}

pub fn code_step(launch: CodeLaunch, surveyed: bool, action: CodeAction) -> CodeStep {
    if launch == CodeLaunch::Survey && !surveyed {
        if let CodeAction::Stop(StopReason::Merged | StopReason::Closed) = action {
            return CodeStep::Action(action);
        }
        if opening_runs_agent(matches!(action, CodeAction::Idle)) {
            return CodeStep::Survey;
        }
    }
    CodeStep::Action(action)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSurvey {
    MergePrompt,
    Poll,
}

pub fn after_opening_survey(merge_ready: bool) -> AfterSurvey {
    if merge_ready {
        AfterSurvey::MergePrompt
    } else {
        AfterSurvey::Poll
    }
}

/// Comment ids present at startup are seen only after that turn succeeds.
pub fn seen_after_survey(ids: &[String], turn_ok: bool) -> Vec<String> {
    if turn_ok {
        ids.to_vec()
    } else {
        Vec::new()
    }
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

/// Why a polled comment was not handed to the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterStats {
    pub untrusted: usize,
    pub empty: usize,
    pub own: usize,
}

impl FilterStats {
    pub fn total(self) -> usize {
        self.untrusted + self.empty + self.own
    }

    /// Every code poll logs this. Nothing dropped is `filtered 0`.
    /// Author-only drops are `filtered 3 (untrusted author)`.
    /// Mixed reasons name each count.
    pub fn log_line(self) -> String {
        if self.total() == 0 {
            return "filtered 0".to_string();
        }
        if self.empty == 0 && self.own == 0 {
            return format!("filtered {} (untrusted author)", self.untrusted);
        }
        let mut reasons = Vec::new();
        if self.untrusted > 0 {
            reasons.push(format!("untrusted author {}", self.untrusted));
        }
        if self.empty > 0 {
            reasons.push(format!("empty {}", self.empty));
        }
        if self.own > 0 {
            reasons.push(format!("own reply {}", self.own));
        }
        format!("filtered {} ({})", self.total(), reasons.join(", "))
    }
}

pub struct FilteredNotes {
    pub kept: Vec<Note>,
    pub stats: FilterStats,
}

pub fn filter_notes(
    notes: &[Note],
    trust: CommentTrust,
    login: &str,
    pr_author: &str,
    repo_owner: &str,
    agent_ids: &BTreeSet<String>,
) -> FilteredNotes {
    let mut kept = Vec::new();
    let mut stats = FilterStats::default();
    for note in notes {
        if note.body.trim().is_empty() {
            stats.empty += 1;
            continue;
        }
        if is_self_generated(note, login, agent_ids) {
            stats.own += 1;
            continue;
        }
        if !note_trusted(note, trust, pr_author, repo_owner) {
            stats.untrusted += 1;
            continue;
        }
        kept.push(note.clone());
    }
    FilteredNotes { kept, stats }
}

pub fn actionable_notes(
    notes: &[Note],
    trust: CommentTrust,
    login: &str,
    pr_author: &str,
    repo_owner: &str,
    agent_ids: &BTreeSet<String>,
) -> Vec<Note> {
    filter_notes(notes, trust, login, pr_author, repo_owner, agent_ids).kept
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
                merge_state: crate::merge::MergeState::Unknown,
                checks,
                approvals: 0,
                changes_requested: false,
            },
            comments_unavailable: false,
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
        assert_eq!(rev(&pr, Some("abc")), ReviewAction::Idle);
        assert_eq!(rev(&pr, None), ReviewAction::Review { full_diff: true });
        assert_eq!(
            rev(&pr, Some("old")),
            ReviewAction::Review { full_diff: false }
        );
    }

    fn rev(pr: &PrSnap, last: Option<&str>) -> ReviewAction {
        decide_review(pr, last, 0, 20, ReviewSignal::default()).unwrap()
    }

    #[test]
    fn review_does_not_stop_just_because_the_pr_is_merge_ready() {
        let mut pr = snap("new", ChecksVerdict::AllGreen);
        pr.merge.merge_state = crate::merge::MergeState::Clean;
        pr.merge.approvals = 1;
        assert!(crate::merge::is_merge_ready(&pr.merge));
        assert_eq!(
            rev(&pr, Some("old")),
            ReviewAction::Review { full_diff: false }
        );
    }

    #[test]
    fn review_opening_turn_runs_even_when_the_sha_was_already_reviewed() {
        let pr = snap("abc", ChecksVerdict::Pending);
        let idle = rev(&pr, Some("abc"));
        assert_eq!(idle, ReviewAction::Idle);
        assert_eq!(
            review_step(ReviewLaunch::Opening, false, idle),
            ReviewStep::Turn
        );
        assert_eq!(
            review_step(ReviewLaunch::Watching, true, rev(&pr, Some("abc"))),
            ReviewStep::Action(ReviewAction::Idle)
        );
        assert_eq!(after_review_verdict(true), AfterVerdict::AskExit);
        assert_eq!(after_review_verdict(false), AfterVerdict::Poll);
        assert_eq!(review_exit_key(ReviewExitKey::Yes), ReviewExitAnswer::Exit);
        assert_eq!(
            review_exit_key(ReviewExitKey::Continue),
            ReviewExitAnswer::Continue
        );
        assert_eq!(
            review_exit_key(ReviewExitKey::Enter),
            ReviewExitAnswer::Exit
        );
        assert_eq!(review_exit_key(ReviewExitKey::Esc), ReviewExitAnswer::Exit);
        assert_eq!(
            review_exit_key(ReviewExitKey::CtrlC),
            ReviewExitAnswer::Exit
        );
        assert_eq!(auto_review_exit(true, true), Some(ReviewExitAnswer::Exit));
        assert_eq!(auto_review_exit(false, false), Some(ReviewExitAnswer::Exit));
        assert_eq!(auto_review_exit(false, true), None);
        assert!(opening_runs_agent(true));
        assert!(opening_runs_agent(false));
        assert!(approve_signal("notes\nGHOST_VERDICT: approve\n"));
        assert!(!approve_signal("LGTM / 承認相当\nGHOST_VERDICT: comment"));
        assert!(approve_signal("posted LGTM / 承認相当"));
        assert!(!approve_signal("looks fine"));
        assert_eq!(
            parse_ghost_verdict("notes\nGHOST_VERDICT: approve\n"),
            Some(ReviewVerdict::Approve)
        );
        assert_eq!(
            parse_ghost_verdict("GHOST_VERDICT: comment\nGHOST_VERDICT: request_changes"),
            Some(ReviewVerdict::RequestChanges)
        );
    }

    #[test]
    fn standalone_code_surveys_before_the_merge_prompt() {
        let mut ready = snap("abc", ChecksVerdict::AllGreen);
        ready.merge.merge_state = crate::merge::MergeState::Clean;
        let action = decide_code(&ready, &BTreeSet::new(), 0, 20, None).unwrap();
        assert!(matches!(action, CodeAction::Stop(StopReason::MergeReady)));
        assert_eq!(
            code_step(CodeLaunch::Survey, false, action),
            CodeStep::Survey
        );
        assert_eq!(after_opening_survey(true), AfterSurvey::MergePrompt);
        assert_eq!(after_opening_survey(false), AfterSurvey::Poll);
        let idle = snap("abc", ChecksVerdict::Pending);
        let idle = decide_code(&idle, &BTreeSet::new(), 0, 20, None).unwrap();
        assert_eq!(
            code_step(CodeLaunch::Handoff, true, idle.clone()),
            CodeStep::Action(idle)
        );
        assert!(seen_after_survey(&["comment:1".into()], false).is_empty());
        assert_eq!(
            seen_after_survey(&["comment:1".into()], true),
            vec!["comment:1".to_string()]
        );
    }

    #[test]
    fn review_does_not_call_agent_at_max_turns() {
        let pr = snap("new", ChecksVerdict::Pending);
        assert!(decide_review(&pr, Some("old"), 20, 20, ReviewSignal::default()).is_err());
        assert!(decide_review(&pr, Some("old"), 3, 3, ReviewSignal::default()).is_err());
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
        let review = decide_review(&pr, Some("abc"), 0, 20, ReviewSignal::default()).unwrap();
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
    fn contributor_is_outside_collaborators_and_inside_contributors() {
        let note = Note {
            id: "inline:4".into(),
            author: "pat".into(),
            body: "please rename this".into(),
            kind: "inline".into(),
            association: "CONTRIBUTOR".into(),
            created_at: String::new(),
            viewer_authored: false,
        };
        let dropped = actionable_notes(
            &[note.clone()],
            CommentTrust::Collaborators,
            "ghost-user",
            "owner",
            "owner",
            &BTreeSet::new(),
        );
        assert!(
            dropped.is_empty(),
            "CONTRIBUTOR is not a collaborator: {dropped:?}"
        );
        let kept = actionable_notes(
            &[note.clone()],
            CommentTrust::Contributors,
            "ghost-user",
            "owner",
            "owner",
            &BTreeSet::new(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "inline:4");
        let member = Note {
            association: "MEMBER".into(),
            id: "inline:5".into(),
            ..note
        };
        assert_eq!(
            actionable_notes(
                &[member],
                CommentTrust::Collaborators,
                "ghost-user",
                "owner",
                "owner",
                &BTreeSet::new(),
            )
            .len(),
            1
        );
    }

    #[test]
    fn default_trust_keeps_copilot_and_outsiders() {
        assert_eq!(CommentTrust::default(), CommentTrust::All);
        let review = Note {
            id: "review:10".into(),
            author: "copilot-pull-request-reviewer[bot]".into(),
            body: "Consider renaming this helper.".into(),
            kind: "review COMMENTED".into(),
            association: "NONE".into(),
            created_at: "2024-06-01T00:05:00Z".into(),
            viewer_authored: false,
        };
        let inline = Note {
            id: "inline:21".into(),
            author: "Copilot".into(),
            body: "src/main.rs:4: this branch is wrong".into(),
            kind: "inline".into(),
            association: "CONTRIBUTOR".into(),
            created_at: "2024-06-01T00:05:02Z".into(),
            viewer_authored: false,
        };
        let outsider = Note {
            id: "comment:3".into(),
            author: "stranger".into(),
            body: "this looks wrong".into(),
            kind: "comment".into(),
            association: "NONE".into(),
            created_at: String::new(),
            viewer_authored: false,
        };
        let notes = [review, inline, outsider.clone()];
        let kept = actionable_notes(
            &notes,
            CommentTrust::default(),
            "cympfh",
            "author",
            "cympfh",
            &BTreeSet::new(),
        );
        assert_eq!(kept.len(), 3, "default trust is all authors: {kept:?}");
        let kept_stats = filter_notes(
            &notes,
            CommentTrust::default(),
            "cympfh",
            "author",
            "cympfh",
            &BTreeSet::new(),
        );
        assert_eq!(kept_stats.stats.log_line(), "filtered 0");
        let restricted = filter_notes(
            &notes,
            CommentTrust::Collaborators,
            "cympfh",
            "author",
            "cympfh",
            &BTreeSet::new(),
        );
        assert!(restricted.kept.is_empty());
        assert_eq!(restricted.stats.log_line(), "filtered 3 (untrusted author)");
        let mut pr = snap("abc", ChecksVerdict::None);
        pr.notes = kept;
        match decide_code(&pr, &BTreeSet::new(), 0, 20, None).unwrap() {
            CodeAction::Fix { new_notes, .. } => {
                assert!(new_notes.iter().any(|n| n.author == "Copilot"));
                assert!(new_notes
                    .iter()
                    .any(|n| n.author == "copilot-pull-request-reviewer[bot]"));
            }
            other => panic!("copilot comments were idle: {other:?}"),
        }
        let _ = outsider;
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
