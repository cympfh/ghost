use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::merge::{
    ci_badge, ci_word, judge_checks, judge_reviews, parse_life, parse_merge_state, parse_mergeable,
    review_badge, MergeInput, ReviewRef,
};
use crate::setup::{SetupFailure, SetupStep};
use crate::shell::{self, CmdOut};
use crate::watch::{Note, PrCandidate, PrSnap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Pull,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenItem {
    pub kind: ItemKind,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
    pub ci: Option<String>,
    pub review: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IssueDetails {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
}

pub struct Gh {
    pub slug: String,
}

impl Gh {
    pub fn new(slug: impl Into<String>) -> Self {
        Self { slug: slug.into() }
    }

    pub fn auth_probe() -> Result<CmdOut> {
        shell::gh(&["auth".into(), "status".into()], None, None)
    }

    pub fn repo_probe(&self) -> Result<CmdOut> {
        self.call(vec![
            "repo".into(),
            "view".into(),
            self.slug.clone(),
            "--json".into(),
            "nameWithOwner".into(),
        ])
    }

    pub fn pr(&self, number: u64) -> Result<PrSnap> {
        let out = self.call(vec![
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            PR_FIELDS.into(),
        ])?;
        ensure_ok("gh pr view", &out)?;
        let mut snap = parse_pr_view(&out.stdout).map_err(|e| anyhow!(e))?;
        let feedback = self.load_feedback(number);
        snap.feedback = feedback.counts;
        snap.feedback_warnings = feedback.warnings;
        if feedback.failed_all {
            snap.notes.clear();
            snap.comments_unavailable = true;
        } else {
            snap.notes = feedback.notes;
        }
        Ok(snap)
    }

    pub fn pr_body(&self, number: u64) -> Result<String> {
        let out = self.call(vec![
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            "body".into(),
        ])?;
        ensure_ok("gh pr view", &out)?;
        let v: Value = serde_json::from_str(&out.stdout).context("parse pr body")?;
        Ok(v.get("body")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub fn set_pr_body(&self, number: u64, body: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "ghost-pr-body-{}-{number}.md",
            self.slug.replace('/', "_")
        ));
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        let out = self.call(vec![
            "pr".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--body-file".into(),
            path.display().to_string(),
        ])?;
        let _ = std::fs::remove_file(&path);
        ensure_ok("gh pr edit", &out)
    }

    /// Real `gh pr merge`. Callers use this from ghost, not from the agent wrapper.
    /// Always passes one method flag and `--match-head-commit`.
    pub fn merge_pr(&self, number: u64, method: Option<&str>, head_sha: &str) -> Result<()> {
        let args = merge_args(&self.slug, number, method, head_sha);
        let out = self.call(args)?;
        ensure_ok("gh pr merge", &out)?;
        Ok(())
    }

    pub fn pr_diff(&self, number: u64) -> Result<String> {
        let out = self.call(vec![
            "pr".into(),
            "diff".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
        ])?;
        ensure_ok("gh pr diff", &out)?;
        Ok(out.stdout)
    }

    pub fn open_items(&self) -> Result<Vec<OpenItem>> {
        let mut items = self.open_prs()?;
        items.extend(self.open_issues()?);
        Ok(items)
    }

    pub fn open_prs(&self) -> Result<Vec<OpenItem>> {
        let out = self.call(vec![
            "pr".into(),
            "list".into(),
            "--repo".into(),
            self.slug.clone(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            "50".into(),
            "--json".into(),
            "number,title,author,labels,reviewDecision,statusCheckRollup,headRefName,url".into(),
        ])?;
        ensure_ok("gh pr list", &out)?;
        parse_pr_list(&out.stdout).map_err(|e| anyhow!(e))
    }

    pub fn open_issues(&self) -> Result<Vec<OpenItem>> {
        let out = self.call(vec![
            "issue".into(),
            "list".into(),
            "--repo".into(),
            self.slug.clone(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            "50".into(),
            "--json".into(),
            "number,title,author,labels,url".into(),
        ])?;
        ensure_ok("gh issue list", &out)?;
        parse_issue_list(&out.stdout).map_err(|e| anyhow!(e))
    }

    /// Open issues, oldest first, for `--auto`. Pages past the first 100.
    pub fn auto_issues(&self) -> Result<Vec<crate::auto::IssueRow>> {
        let (owner, name) = split_slug(&self.slug)?;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..AUTO_PAGE_CAP {
            let out = self.call(graphql_args(ISSUE_QUERY, owner, name, cursor.as_deref()))?;
            ensure_ok("gh issue list", &out)?;
            let page = crate::auto::parse_issue_page(&out.stdout).map_err(|e| anyhow!(e))?;
            let next = crate::auto::next_cursor(page.has_next, page.cursor);
            rows.extend(page.rows);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    }

    /// Pull requests that may already cover an issue, including closed and merged ones.
    pub fn auto_pr_links(&self) -> Result<Vec<crate::auto::PrLink>> {
        let (owner, name) = split_slug(&self.slug)?;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..AUTO_PAGE_CAP {
            let out = self.call(graphql_args(PR_QUERY, owner, name, cursor.as_deref()))?;
            ensure_ok("gh pr list", &out)?;
            let page = crate::auto::parse_pr_page(&out.stdout).map_err(|e| anyhow!(e))?;
            let next = crate::auto::next_cursor(page.has_next, page.cursor);
            rows.extend(page.rows);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    }

    pub fn issue(&self, number: u64) -> Result<IssueDetails> {
        let out = self.call(vec![
            "issue".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            "number,title,body,state,labels".into(),
        ])?;
        ensure_ok("gh issue view", &out)?;
        parse_issue(&out.stdout).map_err(|e| anyhow!(e))
    }

    /// Head branch name for a pull request. Failure is a setup step, not a panic.
    pub fn head_branch(&self, number: u64, purpose: &str) -> Result<String, SetupFailure> {
        let command = format!(
            "gh pr view {number} --repo {} --json headRefName",
            self.slug
        );
        let step = SetupStep {
            intent: format!("{purpose} のブランチ名を知るため"),
            command,
            desired_state: format!(
                "{purpose} の head ブランチ名が分かっている状態にしてから作業して"
            ),
        };
        let out = match self.call(vec![
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            "headRefName".into(),
        ]) {
            Ok(out) => out,
            Err(err) => return Err(SetupFailure::from_step(step, 1, format!("{err:#}"))),
        };
        if out.code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout
            } else {
                out.stderr
            };
            return Err(SetupFailure::from_step(step, out.code, detail));
        }
        let v: Value = match serde_json::from_str(&out.stdout) {
            Ok(v) => v,
            Err(err) => {
                return Err(SetupFailure::from_step(
                    step,
                    1,
                    format!("bad json: {err}\n{}", out.stdout),
                ))
            }
        };
        match v
            .get("headRefName")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(name) => Ok(name.to_string()),
            None => Err(SetupFailure::from_step(step, 1, "headRefName was empty")),
        }
    }

    pub fn default_branch(&self) -> Result<String> {
        let out = self.call(vec![
            "repo".into(),
            "view".into(),
            self.slug.clone(),
            "--json".into(),
            "defaultBranchRef".into(),
        ])?;
        ensure_ok("gh repo view", &out)?;
        let v: Value = serde_json::from_str(&out.stdout).context("parse default branch")?;
        v.pointer("/defaultBranchRef/name")
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("repository has no default branch"))
    }

    pub fn find_issue_pr(&self, issue: u64, branch: &str) -> Result<Option<u64>> {
        let mut candidates = Vec::new();
        let by_head = self.call(vec![
            "pr".into(),
            "list".into(),
            "--repo".into(),
            self.slug.clone(),
            "--head".into(),
            branch.into(),
            "--state".into(),
            "all".into(),
            "--limit".into(),
            "20".into(),
            "--json".into(),
            PR_CANDIDATE_FIELDS.into(),
        ])?;
        if by_head.code == 0 {
            if let Ok(mut found) = parse_pr_candidates(&by_head.stdout) {
                candidates.append(&mut found);
            }
        }
        let issue_view = self.call(vec![
            "issue".into(),
            "view".into(),
            issue.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            "closedByPullRequestsReferences".into(),
        ])?;
        if issue_view.code == 0 {
            candidates.extend(parse_closed_by(&issue_view.stdout, issue));
        }
        let open = self.call(vec![
            "pr".into(),
            "list".into(),
            "--repo".into(),
            self.slug.clone(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            "50".into(),
            "--json".into(),
            PR_CANDIDATE_FIELDS.into(),
        ])?;
        if open.code == 0 {
            if let Ok(mut found) = parse_pr_candidates(&open.stdout) {
                candidates.append(&mut found);
            }
        }
        Ok(crate::watch::find_issue_pr(&candidates, issue, branch).map(|c| c.number))
    }

    fn call(&self, args: Vec<String>) -> Result<CmdOut> {
        shell::gh(&args, None, Some(&self.slug))
    }

    /// Issue comments, review submissions, and inline review comments.
    /// A failed endpoint is a warning; the others are still returned.
    fn load_feedback(&self, number: u64) -> FeedbackLoad {
        let mut warnings = Vec::new();
        let reviews = self.api_list(
            &format!("repos/{}/pulls/{number}/reviews", self.slug),
            "reviews",
            &mut warnings,
        );
        let inline = self.api_list(
            &format!("repos/{}/pulls/{number}/comments", self.slug),
            "inline",
            &mut warnings,
        );
        let comments = self.api_list(
            &format!("repos/{}/issues/{number}/comments", self.slug),
            "comments",
            &mut warnings,
        );
        let failed_all = warnings.len() == 3;
        let counts = crate::watch::FeedbackCounts {
            reviews: reviews.len(),
            inline: inline.len(),
            comments: comments.len(),
        };
        let mut notes = Vec::new();
        for v in &reviews {
            if let Some(note) = note_from_review(v) {
                notes.push(note);
            }
        }
        for v in &inline {
            if let Some(note) = note_from_inline(v) {
                notes.push(note);
            }
        }
        for v in &comments {
            if let Some(note) = note_from_issue_comment(v) {
                notes.push(note);
            }
        }
        FeedbackLoad {
            notes,
            counts,
            warnings,
            failed_all,
        }
    }

    fn api_list(&self, path: &str, label: &str, warnings: &mut Vec<String>) -> Vec<Value> {
        let out = match self.call(vec![
            "api".into(),
            "--paginate".into(),
            "--slurp".into(),
            path.into(),
        ]) {
            Ok(out) => out,
            Err(err) => {
                warnings.push(format!("{label}: {err:#}"));
                return Vec::new();
            }
        };
        if out.code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim()
            } else {
                out.stderr.trim()
            };
            warnings.push(format!("{label}: {detail}"));
            return Vec::new();
        }
        match parse_api_values(&out.stdout) {
            Ok(values) => values,
            Err(err) => {
                warnings.push(format!("{label}: {err}"));
                Vec::new()
            }
        }
    }
}

struct FeedbackLoad {
    notes: Vec<Note>,
    counts: crate::watch::FeedbackCounts,
    warnings: Vec<String>,
    failed_all: bool,
}

const PR_FIELDS: &str = "number,title,state,isDraft,headRefOid,headRefName,baseRefName,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup,reviews,comments,body,author,createdAt,isCrossRepository,headRepository,headRepositoryOwner,maintainerCanModify";
const PR_CANDIDATE_FIELDS: &str = "number,state,headRefName,body,closingIssuesReferences,title";

/// `gh pr merge` refuses to run non-interactively without a method flag.
/// Missing or unknown input is `--squash`. `--merge-method` supplies the override.
pub fn merge_method_flag(method: Option<&str>) -> &'static str {
    match method.unwrap_or("").trim() {
        "--merge" | "merge" => "--merge",
        "--rebase" | "rebase" => "--rebase",
        "--squash" | "squash" | "" => "--squash",
        _ => "--squash",
    }
}

/// Argv for `gh pr merge`. Exactly one of `--merge`, `--squash`, `--rebase`.
pub fn merge_args(slug: &str, number: u64, method: Option<&str>, head_sha: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "merge".into(),
        number.to_string(),
        "--repo".into(),
        slug.to_string(),
        merge_method_flag(method).into(),
        "--match-head-commit".into(),
        head_sha.to_string(),
    ]
}

const AUTO_PAGE_CAP: usize = 20;

const ISSUE_QUERY: &str = "query($owner:String!,$name:String!,$cursor:String){repository(owner:$owner,name:$name){issues(states:OPEN,first:100,after:$cursor,orderBy:{field:CREATED_AT,direction:ASC}){pageInfo{hasNextPage endCursor}nodes{number title createdAt authorAssociation author{__typename login}labels(first:20){nodes{name}}}}}}";

const PR_QUERY: &str = "query($owner:String!,$name:String!,$cursor:String){repository(owner:$owner,name:$name){pullRequests(states:[OPEN,CLOSED,MERGED],first:100,after:$cursor,orderBy:{field:UPDATED_AT,direction:DESC}){pageInfo{hasNextPage endCursor}nodes{number state headRefName headRefOid body closingIssuesReferences(first:50){nodes{number}}}}}}";

fn split_slug(slug: &str) -> Result<(&str, &str)> {
    let (owner, name) = slug
        .split_once('/')
        .ok_or_else(|| anyhow!("bad repo slug {slug}"))?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        anyhow::bail!("bad repo slug {slug}");
    }
    Ok((owner, name))
}

fn graphql_args(query: &str, owner: &str, name: &str, cursor: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={query}"),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("name={name}"),
    ];
    if let Some(cursor) = cursor {
        args.push("-f".into());
        args.push(format!("cursor={cursor}"));
    }
    args
}

fn ensure_ok(what: &str, out: &CmdOut) -> Result<()> {
    if out.code == 0 {
        Ok(())
    } else {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        Err(anyhow!("{what} failed: {detail}"))
    }
}

pub fn parse_pr_view(json: &str) -> Result<PrSnap, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad pr json: {e}"))?;
    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .ok_or("pr json missing number")?;
    let head_sha = v
        .get("headRefOid")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("pr json missing head SHA")?
        .to_string();
    let rollup = v
        .get("statusCheckRollup")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let (checks, failing_checks) = judge_checks(&rollup);
    let reviews = review_refs(v.get("reviews").unwrap_or(&Value::Null));
    let decision = v
        .get("reviewDecision")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let (approvals, changes_requested) = judge_reviews(decision, &reviews);
    let life = parse_life(v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN"));
    let draft = v.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);
    let is_cross = v
        .get("isCrossRepository")
        .and_then(|d| d.as_bool())
        .unwrap_or(false);
    let head_owner = v
        .pointer("/headRepositoryOwner/login")
        .or_else(|| v.pointer("/headRepository/owner/login"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let head_repo = v
        .pointer("/headRepository/name")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let mut head_url = v
        .pointer("/headRepository/sshUrl")
        .or_else(|| v.pointer("/headRepository/url"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if head_url.is_empty() && !head_owner.is_empty() && !head_repo.is_empty() {
        head_url = format!("git@github.com:{head_owner}/{head_repo}.git");
    }
    let maintainer_can_modify = v
        .get("maintainerCanModify")
        .and_then(|d| d.as_bool())
        .unwrap_or(!is_cross);
    let mergeable = parse_mergeable(v.get("mergeable").and_then(|s| s.as_str()).unwrap_or(""));
    let merge_state = parse_merge_state(
        v.get("mergeStateStatus")
            .and_then(|s| s.as_str())
            .unwrap_or(""),
    );
    Ok(PrSnap {
        number,
        title: v
            .get("title")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        life,
        draft,
        head_sha,
        head_branch: v
            .get("headRefName")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        base_branch: v
            .get("baseRefName")
            .and_then(|s| s.as_str())
            .unwrap_or("main")
            .to_string(),
        merge: MergeInput {
            life,
            draft,
            mergeable,
            merge_state,
            checks,
            approvals,
            changes_requested,
        },
        failing_checks,
        notes: notes_from(
            v.get("reviews").unwrap_or(&Value::Null),
            v.get("comments").unwrap_or(&Value::Null),
        ),
        ci_label: ci_word(checks).to_string(),
        author: v
            .pointer("/author/login")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: v
            .get("createdAt")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        feedback: crate::watch::FeedbackCounts::default(),
        feedback_warnings: Vec::new(),
        comments_unavailable: false,
        is_cross,
        head_owner,
        head_repo,
        head_url,
        maintainer_can_modify,
    })
}

fn review_refs(reviews: &Value) -> Vec<ReviewRef> {
    let Some(arr) = reviews.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .map(|r| ReviewRef {
            author: login(r.get("author")),
            state: r
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            submitted_at: r
                .get("submittedAt")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

fn notes_from(reviews: &Value, comments: &Value) -> Vec<Note> {
    let mut notes = Vec::new();
    if let Some(arr) = reviews.as_array() {
        for review in arr {
            if let Some(note) = note_from_review(review) {
                notes.push(note);
            }
        }
    }
    if let Some(arr) = comments.as_array() {
        for comment in arr {
            if let Some(note) = note_from_issue_comment(comment) {
                notes.push(note);
            }
        }
    }
    notes
}

pub fn parse_api_values(json: &str) -> Result<Vec<Value>, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => Ok(flatten_pages(value)),
        Err(err) => {
            let mut out = Vec::new();
            let mut any = false;
            for line in trimmed.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(line)
                    .map_err(|line_err| format!("bad gh api json: {line_err}"))?;
                any = true;
                out.extend(flatten_pages(value));
            }
            if any {
                Ok(out)
            } else {
                Err(format!("bad gh api json: {err}"))
            }
        }
    }
}

fn flatten_pages(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items)
            if items.iter().any(Value::is_array) && items.iter().all(Value::is_array) =>
        {
            items.into_iter().flat_map(flatten_pages).collect()
        }
        Value::Array(items) => items,
        other => vec![other],
    }
}

fn note_from_review(v: &Value) -> Option<Note> {
    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    if matches!(state.as_str(), "" | "APPROVED" | "DISMISSED" | "PENDING") {
        return None;
    }
    let body = text_field(v, &["body"]);
    if body.is_empty() {
        return None;
    }
    let id = json_id(v)?;
    Some(finish_note(
        v,
        format!("review:{id}"),
        body,
        format!("review {state}"),
        &["submitted_at", "submittedAt"],
    ))
}

fn note_from_inline(v: &Value) -> Option<Note> {
    let body = text_field(v, &["body"]);
    if body.is_empty() {
        return None;
    }
    let path = v.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let line = v
        .get("line")
        .and_then(|n| n.as_u64())
        .or_else(|| v.get("original_line").and_then(|n| n.as_u64()));
    let shown = match (path.is_empty(), line) {
        (false, Some(n)) => format!("{path}:{n}: {body}"),
        (false, None) => format!("{path}: {body}"),
        _ => body,
    };
    let id = json_id(v)?;
    Some(finish_note(
        v,
        format!("inline:{id}"),
        shown,
        "inline".into(),
        &["created_at", "createdAt", "updated_at"],
    ))
}

fn note_from_issue_comment(v: &Value) -> Option<Note> {
    let body = text_field(v, &["body"]);
    if body.is_empty() {
        return None;
    }
    let id = format!("comment:{}", json_id(v)?);
    Some(finish_note(
        v,
        id,
        body,
        "comment".into(),
        &["created_at", "createdAt", "updated_at"],
    ))
}

fn finish_note(v: &Value, id: String, body: String, kind: String, when: &[&str]) -> Note {
    let author = user_login(v);
    Note {
        id,
        author: author.clone(),
        body,
        kind,
        association: association_of(v),
        created_at: when
            .iter()
            .find_map(|key| v.get(*key).and_then(|s| s.as_str()))
            .unwrap_or("")
            .to_string(),
        viewer_authored: v
            .get("viewerDidAuthor")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
    }
}

fn text_field(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| v.get(*key).and_then(|s| s.as_str()))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn json_id(v: &Value) -> Option<String> {
    if let Some(n) = v.get("databaseId").and_then(|x| x.as_u64()) {
        return Some(n.to_string());
    }
    if let Some(n) = v.get("id").and_then(|x| x.as_u64()) {
        return Some(n.to_string());
    }
    v.get("id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
}

fn user_login(v: &Value) -> String {
    v.pointer("/user/login")
        .or_else(|| v.pointer("/author/login"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| login(v.get("author")))
}

fn association_of(v: &Value) -> String {
    v.get("author_association")
        .or_else(|| v.get("authorAssociation"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

fn login(author: Option<&Value>) -> String {
    author
        .and_then(|a| a.get("login"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

fn labels_of(v: &Value) -> Vec<String> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_pr_list(json: &str) -> Result<Vec<OpenItem>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad pr list: {e}"))?;
    let mut items = Vec::new();
    for v in arr {
        let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        let rollup = v
            .get("statusCheckRollup")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let (checks, _) = judge_checks(&rollup);
        let decision = v
            .get("reviewDecision")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let (approvals, changes) = judge_reviews(decision, &[]);
        items.push(OpenItem {
            kind: ItemKind::Pull,
            number,
            title: v
                .get("title")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            author: login(v.get("author")),
            labels: labels_of(&v),
            ci: Some(ci_badge(checks).to_string()),
            review: Some(review_badge(decision, changes, approvals)),
        });
    }
    Ok(items)
}

pub fn parse_issue_list(json: &str) -> Result<Vec<OpenItem>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad issue list: {e}"))?;
    let mut items = Vec::new();
    for v in arr {
        let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        items.push(OpenItem {
            kind: ItemKind::Issue,
            number,
            title: v
                .get("title")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            author: login(v.get("author")),
            labels: labels_of(&v),
            ci: None,
            review: None,
        });
    }
    Ok(items)
}

pub fn parse_issue(json: &str) -> Result<IssueDetails, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad issue json: {e}"))?;
    Ok(IssueDetails {
        number: v.get("number").and_then(|n| n.as_u64()).unwrap_or(0),
        title: v
            .get("title")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        body: v
            .get("body")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        labels: labels_of(&v),
    })
}

pub fn parse_pr_candidates(json: &str) -> Result<Vec<PrCandidate>, String> {
    let arr: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("bad pr candidates: {e}"))?;
    Ok(arr
        .iter()
        .filter_map(|v| candidate_from_pr(v, None))
        .collect())
}

fn candidate_from_pr(v: &Value, force_issue: Option<u64>) -> Option<PrCandidate> {
    let number = v.get("number").and_then(|n| n.as_u64())?;
    let mut closing = closing_numbers(v.get("closingIssuesReferences"));
    if let Some(issue) = force_issue {
        if !closing.contains(&issue) {
            closing.push(issue);
        }
    }
    Some(PrCandidate {
        number,
        state: v
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("OPEN")
            .to_string(),
        head_ref: v
            .get("headRefName")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        body: v
            .get("body")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        closing_numbers: closing,
    })
}

fn closing_numbers(v: Option<&Value>) -> Vec<u64> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            item.get("number")
                .and_then(|n| n.as_u64())
                .or_else(|| item.as_u64())
        })
        .collect()
}

pub fn parse_closed_by(json: &str, issue: u64) -> Vec<PrCandidate> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let Some(arr) = v
        .get("closedByPullRequestsReferences")
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| candidate_from_pr(item, Some(issue)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::{ChecksVerdict, Mergeability};

    #[test]
    fn parses_pr_view_fixture() {
        let json = r#"{
            "number": 3,
            "title": "Add ghost",
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": "abcdef1234567890",
            "headRefName": "fix/3-crash",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [
                {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"FAILURE"}
            ],
            "reviews": [
                {"id":"PRR_1","databaseId":10,"author":{"login":"ada"},"state":"CHANGES_REQUESTED","body":"rename this","submittedAt":"2024-01-02T00:00:00Z","viewerDidAuthor":false},
                {"id":"PRR_2","databaseId":11,"author":{"login":"me"},"state":"COMMENTED","body":"on it","submittedAt":"2024-01-03T00:00:00Z","viewerDidAuthor":true}
            ],
            "comments": [
                {"id":"IC_1","databaseId":123,"author":{"login":"bea"},"body":"also the tests","viewerDidAuthor":false},
                {"id":"IC_kwABC","author":{"login":"bea"},"body":"graphql only","viewerDidAuthor":false}
            ]
        }"#;
        let pr = parse_pr_view(json).unwrap();
        assert_eq!(pr.head_sha, "abcdef1234567890");
        assert_eq!(pr.merge.checks, ChecksVerdict::Failed);
        assert_eq!(pr.failing_checks, vec!["build".to_string()]);
        assert!(pr.merge.changes_requested);
        assert_eq!(pr.merge.mergeable, Mergeability::Mergeable);
        assert_eq!(pr.notes.len(), 3);
        assert_eq!(pr.notes[0].id, "review:10");
        assert!(pr.notes.iter().any(|n| n.viewer_authored));
        assert!(pr.notes.iter().any(|n| n.id == "comment:123"));
        assert!(pr.notes.iter().all(|n| n.id != "comment:IC_kwABC"));
        assert!(!pr.is_cross);
        assert!(pr.maintainer_can_modify);

        let fork = r#"{
            "number": 9,
            "state": "OPEN",
            "headRefOid": "bbbbbbbb",
            "headRefName": "main",
            "isCrossRepository": true,
            "maintainerCanModify": false,
            "headRepositoryOwner": {"login": "forker"},
            "headRepository": {"name": "app", "sshUrl": "git@github.com:forker/app.git"}
        }"#;
        let fork = parse_pr_view(fork).unwrap();
        assert!(fork.is_cross);
        assert!(!fork.maintainer_can_modify);
        assert_eq!(fork.head_owner, "forker");
        assert_eq!(fork.head_repo, "app");
        assert_eq!(fork.head_url, "git@github.com:forker/app.git");
        assert_eq!(fork.head_branch, "main");

        let no_url = r#"{
            "number": 9,
            "state": "OPEN",
            "headRefOid": "bbbbbbbb",
            "headRefName": "feature",
            "isCrossRepository": true,
            "maintainerCanModify": true,
            "headRepositoryOwner": {"login": "forker"},
            "headRepository": {"name": "app"}
        }"#;
        let no_url = parse_pr_view(no_url).unwrap();
        assert_eq!(no_url.head_url, "git@github.com:forker/app.git");
    }

    #[test]
    fn parses_review_inline_and_issue_comments() {
        let reviews = r#"[[
            {"id":10,"user":{"login":"cympfh"},"body":"この分岐を直して","state":"COMMENTED","author_association":"OWNER","submitted_at":"2024-06-01T00:05:00Z"},
            {"id":11,"user":{"login":"cympfh"},"body":"","state":"COMMENTED","author_association":"OWNER","submitted_at":"2024-06-01T00:05:01Z"}
        ]]"#;
        let inline = r#"[
            {"id":21,"user":{"login":"cympfh"},"body":"rename this","author_association":"OWNER","path":"src/main.rs","line":4,"created_at":"2024-06-01T00:05:02Z"},
            {"id":22,"user":{"login":"cympfh"},"body":"and this","author_association":"OWNER","path":"src/lib.rs","line":8,"created_at":"2024-06-01T00:05:03Z"}
        ]"#;
        let comments = "[]";
        let reviews = parse_api_values(reviews).unwrap();
        let inline = parse_api_values(inline).unwrap();
        let comments = parse_api_values(comments).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(inline.len(), 2);
        assert!(comments.is_empty());
        let notes: Vec<_> = reviews
            .iter()
            .filter_map(note_from_review)
            .chain(inline.iter().filter_map(note_from_inline))
            .collect();
        assert_eq!(
            notes.len(),
            3,
            "empty review body is not a note; inline comments are"
        );
        assert_eq!(notes[0].id, "review:10");
        assert_eq!(notes[0].association, "OWNER");
        assert!(!notes[0].viewer_authored);
        assert_eq!(notes[1].kind, "inline");
        assert!(notes[1].body.contains("src/main.rs:4"));
        assert_eq!(
            crate::watch::feedback_log(crate::watch::FeedbackCounts {
                reviews: reviews.len(),
                inline: inline.len(),
                comments: comments.len(),
            }),
            "reviews 2, inline 2, comments 0"
        );
    }

    #[test]
    fn parses_copilot_review_and_inline_comment() {
        // Reviews use copilot-pull-request-reviewer[bot]; inline comments use Copilot.
        // author_association is NONE or CONTRIBUTOR, user.type is Bot.
        let reviews = r#"[
            {
              "id": 1001,
              "user": {"login": "copilot-pull-request-reviewer[bot]", "id": 175728472, "type": "Bot"},
              "body": "Consider renaming this helper.",
              "state": "COMMENTED",
              "author_association": "NONE",
              "submitted_at": "2024-06-02T00:00:00Z"
            }
        ]"#;
        let inline = r#"[
            {
              "id": 2002,
              "user": {"login": "Copilot", "id": 175728472, "type": "Bot"},
              "body": "this branch is wrong",
              "author_association": "CONTRIBUTOR",
              "path": "src/main.rs",
              "line": 4,
              "created_at": "2024-06-02T00:00:01Z"
            },
            {
              "id": 2003,
              "user": {"login": "copilot-pull-request-reviewer[bot]", "id": 175728472, "type": "Bot"},
              "body": "inline from the review bot",
              "author_association": "NONE",
              "path": "src/lib.rs",
              "line": 8,
              "created_at": "2024-06-02T00:00:02Z"
            }
        ]"#;
        let reviews = parse_api_values(reviews).unwrap();
        let inline = parse_api_values(inline).unwrap();
        let notes: Vec<_> = reviews
            .iter()
            .filter_map(note_from_review)
            .chain(inline.iter().filter_map(note_from_inline))
            .collect();
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].author, "copilot-pull-request-reviewer[bot]");
        assert_eq!(notes[0].association, "NONE");
        assert_eq!(notes[0].id, "review:1001");
        assert_eq!(notes[1].author, "Copilot");
        assert_eq!(notes[1].association, "CONTRIBUTOR");
        assert!(notes[1].body.contains("src/main.rs:4"));
        assert_eq!(notes[2].author, "copilot-pull-request-reviewer[bot]");
        assert_eq!(notes[2].kind, "inline");
        let kept = crate::watch::filter_notes(
            &notes,
            crate::watch::CommentTrust::default(),
            "cympfh",
            "author",
            "cympfh",
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(kept.kept.len(), 3);
        assert_eq!(kept.stats.log_line(), "filtered 0");
        let dropped = crate::watch::filter_notes(
            &notes,
            crate::watch::CommentTrust::Collaborators,
            "cympfh",
            "author",
            "cympfh",
            &std::collections::BTreeSet::new(),
        );
        assert!(dropped.kept.is_empty());
        assert_eq!(dropped.stats.log_line(), "filtered 3 (untrusted author)");
    }

    #[test]
    fn parses_open_lists() {
        let prs = r#"[{"number":12,"title":"Fix login","author":{"login":"ada"},"labels":[{"name":"bug"}],"reviewDecision":"APPROVED","statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}]"#;
        let items = parse_pr_list(prs).unwrap();
        assert_eq!(items[0].ci.as_deref(), Some("CI ✓"));
        assert_eq!(items[0].review.as_deref(), Some("approved"));
        assert_eq!(items[0].labels, vec!["bug".to_string()]);
        assert_eq!(items[0].author, "ada");

        let issues = r#"[{"number":7,"title":"Crash","author":{"login":"mei"},"labels":[]}]"#;
        let issues = parse_issue_list(issues).unwrap();
        assert_eq!(issues[0].kind, ItemKind::Issue);
        assert!(issues[0].ci.is_none());
    }

    #[test]
    fn merge_argv_always_contains_exactly_one_method_flag() {
        let methods = [
            None,
            Some(""),
            Some("--squash"),
            Some("squash"),
            Some("--merge"),
            Some("merge"),
            Some("--rebase"),
            Some("rebase"),
            Some("not-a-method"),
        ];
        for method in methods {
            let args = merge_args("acme/app", 3, method, "abc123def");
            let flags: Vec<_> = args
                .iter()
                .filter(|a| matches!(a.as_str(), "--merge" | "--squash" | "--rebase"))
                .collect();
            assert_eq!(flags.len(), 1, "method {method:?} argv {args:?}");
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "--match-head-commit" && w[1] == "abc123def"),
                "{args:?}"
            );
        }
        assert_eq!(
            merge_method_flag(None),
            "--squash",
            "default method is squash"
        );
        assert!(merge_args("acme/app", 3, None, "abc").contains(&"--squash".to_string()));
        let merge = merge_args("acme/app", 3, Some("--merge"), "abc");
        assert!(merge.contains(&"--merge".to_string()));
        assert!(!merge.iter().any(|a| a == "--squash" || a == "--rebase"));
        let rebase = merge_args("acme/app", 3, Some("--rebase"), "abc");
        assert!(rebase.contains(&"--rebase".to_string()));
        assert!(!rebase.iter().any(|a| a == "--squash" || a == "--merge"));
    }
}
