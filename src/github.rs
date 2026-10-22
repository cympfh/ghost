use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::merge::{
    ci_badge, ci_word, judge_checks, judge_reviews, parse_life, parse_mergeable, review_badge,
    MergeInput, ReviewRef,
};
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
        parse_pr_view(&out.stdout).map_err(|e| anyhow!(e))
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

    pub fn issue(&self, number: u64) -> Result<IssueDetails> {
        let out = self.call(vec![
            "issue".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            self.slug.clone(),
            "--json".into(),
            "number,title,body,state".into(),
        ])?;
        ensure_ok("gh issue view", &out)?;
        parse_issue(&out.stdout).map_err(|e| anyhow!(e))
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
}

const PR_FIELDS: &str = "number,title,state,isDraft,headRefOid,headRefName,baseRefName,mergeable,reviewDecision,statusCheckRollup,reviews,comments,body";
const PR_CANDIDATE_FIELDS: &str = "number,state,headRefName,body,closingIssuesReferences,title";

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
    let mergeable = parse_mergeable(v.get("mergeable").and_then(|s| s.as_str()).unwrap_or(""));
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
            if review.get("viewerDidAuthor").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            let state = review
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_ascii_uppercase();
            if matches!(state.as_str(), "APPROVED" | "DISMISSED" | "PENDING") {
                continue;
            }
            let body = review
                .get("body")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if body.is_empty() {
                continue;
            }
            let author = login(review.get("author"));
            let id = note_id(review, "review", &author, &body);
            notes.push(Note {
                id,
                author,
                body,
                kind: format!("review {state}"),
            });
        }
    }
    if let Some(arr) = comments.as_array() {
        for comment in arr {
            if comment.get("viewerDidAuthor").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            let body = comment
                .get("body")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if body.is_empty() {
                continue;
            }
            let author = login(comment.get("author"));
            let id = note_id(comment, "comment", &author, &body);
            notes.push(Note {
                id,
                author,
                body,
                kind: "comment".into(),
            });
        }
    }
    notes
}

fn note_id(v: &Value, kind: &str, author: &str, body: &str) -> String {
    let id = v.get("id").and_then(|s| s.as_str()).unwrap_or("");
    if !id.is_empty() {
        return id.to_string();
    }
    let stamp = v
        .get("submittedAt")
        .or_else(|| v.get("createdAt"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let snippet: String = body.chars().take(40).collect();
    format!("{kind}:{author}:{stamp}:{snippet}")
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
            "headRefName": "ghost/issue-3",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [
                {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"FAILURE"}
            ],
            "reviews": [
                {"id":"PRR_1","author":{"login":"ada"},"state":"CHANGES_REQUESTED","body":"rename this","submittedAt":"2024-01-02T00:00:00Z","viewerDidAuthor":false},
                {"id":"PRR_2","author":{"login":"me"},"state":"COMMENTED","body":"on it","submittedAt":"2024-01-03T00:00:00Z","viewerDidAuthor":true}
            ],
            "comments": [
                {"id":"IC_1","author":{"login":"bea"},"body":"also the tests","viewerDidAuthor":false}
            ]
        }"#;
        let pr = parse_pr_view(json).unwrap();
        assert_eq!(pr.head_sha, "abcdef1234567890");
        assert_eq!(pr.merge.checks, ChecksVerdict::Failed);
        assert_eq!(pr.failing_checks, vec!["build".to_string()]);
        assert!(pr.merge.changes_requested);
        assert_eq!(pr.merge.mergeable, Mergeability::Mergeable);
        assert_eq!(pr.notes.len(), 2);
        assert_eq!(pr.notes[0].id, "PRR_1");
        assert_eq!(pr.notes[1].id, "IC_1");
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
}
