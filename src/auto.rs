//! Choose the next open issue for `--auto`.
//!
//! An empty result waits for the next check. It does not end the process.

use serde_json::Value;

use crate::state::AutoFailure;
use crate::watch::{association_trusted, body_closes_issue, CommentTrust};
use crate::workdir;

/// One open issue from the issue query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub created_at: String,
    pub author: String,
    pub association: String,
    pub is_bot: bool,
}

/// A pull request that may already cover an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLife {
    Open,
    Closed,
    Merged,
}

impl PrLife {
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_uppercase().as_str() {
            "OPEN" => Self::Open,
            "MERGED" => Self::Merged,
            "CLOSED" => Self::Closed,
            "" => Self::Open,
            _ => Self::Closed,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLink {
    pub number: u64,
    pub head_ref: String,
    pub head_oid: String,
    pub body: String,
    pub closing: Vec<u64>,
    pub state: PrLife,
}

/// Why an older issue was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    Failed {
        number: u64,
        reason: String,
    },
    OpenPr {
        number: u64,
        pr: u64,
    },
    Closed {
        number: u64,
        pr: u64,
    },
    Untrusted {
        number: u64,
        author: String,
        association: String,
    },
    /// The agent ran `gh issue create` and the wrapper recorded this number.
    AgentCreated {
        number: u64,
    },
    Backoff {
        number: u64,
        retry_at: String,
    },
    Busy {
        number: u64,
    },
}

/// What one check of the open-issue list should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoAction {
    /// Run the existing issue flow for this number. One issue at a time.
    Run(u64),
    /// Nothing eligible. Keep waiting for the next check.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub action: AutoAction,
    pub skipped: Vec<Skip>,
}

/// Inputs for one oldest-first check.
pub struct PollIn<'a> {
    pub issues: &'a [IssueRow],
    pub failed: &'a [AutoFailure],
    pub prs: &'a [PrLink],
    pub trust: CommentTrust,
    /// Issue numbers the agent created with `gh issue create`.
    pub created: &'a [u64],
    pub now: &'a str,
    pub busy: &'a [u64],
}

/// Oldest eligible issue. Dated issues come before ones with an empty timestamp.
pub fn poll(input: &PollIn<'_>) -> Poll {
    let mut rows: Vec<&IssueRow> = input.issues.iter().filter(|row| row.number > 0).collect();
    rows.sort_by_key(|row| oldest_key(row));
    let mut skipped = Vec::new();
    for row in rows {
        if input.busy.contains(&row.number) {
            skipped.push(Skip::Busy { number: row.number });
            continue;
        }
        if input.created.contains(&row.number) {
            skipped.push(Skip::AgentCreated { number: row.number });
            continue;
        }
        if !association_trusted(&row.association, input.trust) {
            skipped.push(Skip::Untrusted {
                number: row.number,
                author: row.author.clone(),
                association: row.association.clone(),
            });
            continue;
        }
        let links = linked(row, input.prs);
        let open = links.iter().find(|pr| pr.state.is_open()).copied();
        let closed = links.iter().find(|pr| !pr.state.is_open()).copied();
        if open.is_none() {
            if let Some(pr) = closed {
                skipped.push(Skip::Closed {
                    number: row.number,
                    pr: pr.number,
                });
                continue;
            }
        }
        if let Some(hold) = input
            .failed
            .iter()
            .find(|row_fail| row_fail.number == row.number)
        {
            if !hold.transient {
                skipped.push(Skip::Failed {
                    number: row.number,
                    reason: hold.reason.clone(),
                });
                continue;
            }
            let moved = head_moved(hold, row, input.prs);
            let due = hold.retry_at.is_empty() || input.now >= hold.retry_at.as_str();
            if !moved && !due {
                skipped.push(Skip::Backoff {
                    number: row.number,
                    retry_at: hold.retry_at.clone(),
                });
                continue;
            }
            // A due retry, or a new head, continues the open pull request.
            return Poll {
                action: AutoAction::Run(row.number),
                skipped,
            };
        }
        if let Some(pr) = open {
            skipped.push(Skip::OpenPr {
                number: row.number,
                pr: pr.number,
            });
            continue;
        }
        return Poll {
            action: AutoAction::Run(row.number),
            skipped,
        };
    }
    Poll {
        action: AutoAction::Idle,
        skipped,
    }
}

/// Several idle checks in a row. There is no exit step.
pub fn idle_waits(polls: &[Poll]) -> bool {
    !polls.is_empty() && polls.iter().all(|poll| poll.action == AutoAction::Idle)
}

pub fn current_head(issue: &IssueRow, prs: &[PrLink]) -> String {
    let links = linked(issue, prs);
    links
        .iter()
        .find(|pr| pr.state.is_open())
        .or_else(|| links.first())
        .map(|pr| pr.head_oid.clone())
        .unwrap_or_default()
}

/// `502`, `503`, `504`, `429`, timeouts, and connection failures are retried.
pub fn is_transient(reason: &str) -> bool {
    let text = reason.to_ascii_lowercase();
    [
        "502",
        "503",
        "504",
        "429",
        "timeout",
        "timed out",
        "rate limit",
        "connection",
        "temporarily",
        "temporary",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

pub fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;
    let sec = tod % 60;
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn oldest_key(row: &IssueRow) -> (bool, String, u64) {
    (
        row.created_at.is_empty(),
        row.created_at.clone(),
        row.number,
    )
}

fn head_moved(hold: &AutoFailure, issue: &IssueRow, prs: &[PrLink]) -> bool {
    if hold.head.is_empty() {
        return false;
    }
    let current = current_head(issue, prs);
    !current.is_empty() && current != hold.head
}

fn linked<'a>(issue: &IssueRow, prs: &'a [PrLink]) -> Vec<&'a PrLink> {
    let labels: Vec<&str> = issue.labels.iter().map(String::as_str).collect();
    let branch = workdir::issue_branch(issue.number, &issue.title, &labels);
    prs.iter()
        .filter(|pr| {
            pr.head_ref == branch
                || pr.closing.contains(&issue.number)
                || body_closes_issue(&pr.body, issue.number)
        })
        .collect()
}

/// Howard Hinnant's `civil_from_days`. `z` is days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

#[derive(Debug)]
pub struct IssuePage {
    pub rows: Vec<IssueRow>,
    pub has_next: bool,
    pub cursor: Option<String>,
}

#[derive(Debug)]
pub struct PrPage {
    pub rows: Vec<PrLink>,
    pub has_next: bool,
    pub cursor: Option<String>,
}

/// Next page cursor. `None` means this page is the last one.
pub fn next_cursor(has_next: bool, cursor: Option<String>) -> Option<String> {
    if has_next {
        cursor.filter(|cursor| !cursor.is_empty())
    } else {
        None
    }
}

pub fn parse_issues(json: &str) -> Result<Vec<IssueRow>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad issue list: {e}"))?;
    let mut rows = Vec::new();
    for v in arr {
        if let Some(row) = parse_issue_node(&v) {
            rows.push(row);
        }
    }
    Ok(rows)
}

pub fn parse_issue_page(json: &str) -> Result<IssuePage, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad issue list: {e}"))?;
    if let Some(msg) = graphql_error(&v) {
        return Err(msg);
    }
    let issues = v
        .pointer("/data/repository/issues")
        .ok_or_else(|| "issue list missing data.repository.issues".to_string())?;
    let (rows, has_next, cursor) = parse_connection(issues, parse_issue_node)?;
    Ok(IssuePage {
        rows,
        has_next,
        cursor,
    })
}

pub fn parse_pr_links(json: &str) -> Result<Vec<PrLink>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad pr list: {e}"))?;
    let mut rows = Vec::new();
    for v in arr {
        if let Some(row) = parse_pr_node(&v) {
            rows.push(row);
        }
    }
    Ok(rows)
}

pub fn parse_pr_page(json: &str) -> Result<PrPage, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad pr list: {e}"))?;
    if let Some(msg) = graphql_error(&v) {
        return Err(msg);
    }
    let prs = v
        .pointer("/data/repository/pullRequests")
        .ok_or_else(|| "pr list missing data.repository.pullRequests".to_string())?;
    let (rows, has_next, cursor) = parse_connection(prs, parse_pr_node)?;
    Ok(PrPage {
        rows,
        has_next,
        cursor,
    })
}

fn parse_connection<T>(
    connection: &Value,
    parse_node: impl Fn(&Value) -> Option<T>,
) -> Result<(Vec<T>, bool, Option<String>), String> {
    let nodes = connection
        .get("nodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "page missing nodes".to_string())?;
    let rows = nodes.iter().filter_map(parse_node).collect();
    let page = connection.get("pageInfo").cloned().unwrap_or(Value::Null);
    let has_next = page
        .get("hasNextPage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cursor = page
        .get("endCursor")
        .and_then(|v| v.as_str())
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    Ok((rows, has_next, cursor))
}

fn graphql_error(v: &Value) -> Option<String> {
    let errors = v.get("errors")?.as_array()?;
    let first = errors.first()?;
    let msg = first
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("graphql error");
    Some(msg.to_string())
}

fn parse_issue_node(v: &Value) -> Option<IssueRow> {
    let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
    if number == 0 {
        return None;
    }
    let (author, is_bot) = author_of(v);
    Some(IssueRow {
        number,
        title: text(v, "title"),
        labels: names(v.get("labels")),
        created_at: text(v, "createdAt"),
        author,
        association: association_of(v),
        is_bot,
    })
}

fn parse_pr_node(v: &Value) -> Option<PrLink> {
    let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
    if number == 0 {
        return None;
    }
    Some(PrLink {
        number,
        head_ref: text(v, "headRefName"),
        head_oid: text(v, "headRefOid"),
        body: text(v, "body"),
        closing: closing_numbers(v.get("closingIssuesReferences")),
        state: PrLife::parse(v.get("state").and_then(|s| s.as_str()).unwrap_or("")),
    })
}

fn author_of(v: &Value) -> (String, bool) {
    let Some(author) = v.get("author").filter(|author| !author.is_null()) else {
        return (String::new(), false);
    };
    let login = author
        .get("login")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let typename = author
        .get("__typename")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let is_bot = author
        .get("is_bot")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
        || typename.eq_ignore_ascii_case("Bot");
    (login, is_bot)
}

fn association_of(v: &Value) -> String {
    v.get("authorAssociation")
        .or_else(|| v.get("author_association"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

fn names(v: Option<&Value>) -> Vec<String> {
    let Some(v) = v else {
        return Vec::new();
    };
    let arr = v
        .get("nodes")
        .and_then(|nodes| nodes.as_array())
        .or_else(|| v.as_array());
    arr.map(|arr| {
        arr.iter()
            .filter_map(|item| {
                item.get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
            })
            .collect()
    })
    .unwrap_or_default()
}

fn closing_numbers(v: Option<&Value>) -> Vec<u64> {
    let Some(v) = v else {
        return Vec::new();
    };
    let arr = v
        .get("nodes")
        .and_then(|nodes| nodes.as_array())
        .or_else(|| v.as_array());
    let Some(arr) = arr else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64, created_at: &str, title: &str) -> IssueRow {
        IssueRow {
            number,
            title: title.into(),
            labels: Vec::new(),
            created_at: created_at.into(),
            author: "ada".into(),
            association: "OWNER".into(),
            is_bot: false,
        }
    }

    fn sample() -> Vec<IssueRow> {
        vec![
            issue(2, "2020-03-01T00:00:00Z", "Later"),
            issue(9, "2020-01-01T00:00:00Z", "Oldest"),
            issue(1, "", "Undated"),
        ]
    }

    fn choose(issues: &[IssueRow], failed: &[AutoFailure], prs: &[PrLink]) -> Poll {
        poll(&PollIn {
            issues,
            failed,
            prs,
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-01-01T00:00:00Z",
            busy: &[],
        })
    }

    fn open_pr(number: u64, head_ref: &str, body: &str, closing: Vec<u64>) -> PrLink {
        PrLink {
            number,
            head_ref: head_ref.into(),
            head_oid: "abc".into(),
            body: body.into(),
            closing,
            state: PrLife::Open,
        }
    }

    #[test]
    fn oldest_dated_issue_is_first_and_only_one_is_chosen() {
        let chosen = choose(&sample(), &[], &[]);
        assert_eq!(chosen.action, AutoAction::Run(9));
        assert!(chosen.skipped.is_empty());
    }

    #[test]
    fn skips_failures_then_open_prs_and_leaves_the_rest() {
        let branch = workdir::issue_branch(2, "Later", &[]);
        let prs = vec![
            open_pr(40, "unrelated", "see #2", vec![]),
            open_pr(41, &branch, "", vec![]),
        ];
        let failed = vec![AutoFailure {
            number: 9,
            reason: "no pull request".into(),
            transient: false,
            retry_at: String::new(),
            head: String::new(),
            attempts: 1,
        }];
        let chosen = choose(&sample(), &failed, &prs);
        assert_eq!(chosen.action, AutoAction::Run(1));
        assert_eq!(
            chosen.skipped,
            vec![
                Skip::Failed {
                    number: 9,
                    reason: "no pull request".into(),
                },
                Skip::OpenPr { number: 2, pr: 41 },
            ]
        );
    }

    #[test]
    fn open_pr_link_in_the_body_or_closing_list_skips_the_issue() {
        let prs = vec![open_pr(8, "feature", "Closes #9", vec![1])];
        let chosen = choose(&sample(), &[], &prs);
        assert_eq!(chosen.action, AutoAction::Run(2));
        assert_eq!(chosen.skipped, vec![Skip::OpenPr { number: 9, pr: 8 }]);
        let only_undated = choose(&[issue(1, "", "Undated")], &[], &prs);
        assert_eq!(only_undated.action, AutoAction::Idle);
        assert_eq!(
            only_undated.skipped,
            vec![Skip::OpenPr { number: 1, pr: 8 }]
        );
    }

    #[test]
    fn a_pull_request_that_is_not_open_does_not_skip_when_unrelated() {
        let chosen = choose(&sample(), &[], &[]);
        assert_eq!(chosen.action, AutoAction::Run(9));
    }

    #[test]
    fn a_closed_pull_request_is_not_redone() {
        let prs = vec![PrLink {
            number: 8,
            head_ref: "feature".into(),
            head_oid: "abc".into(),
            body: "Closes #9".into(),
            closing: vec![],
            state: PrLife::Closed,
        }];
        let chosen = choose(&sample(), &[], &prs);
        assert_eq!(chosen.action, AutoAction::Run(2));
        assert_eq!(chosen.skipped, vec![Skip::Closed { number: 9, pr: 8 }]);
        let merged = vec![PrLink {
            state: PrLife::Merged,
            ..prs[0].clone()
        }];
        let chosen = choose(&[issue(9, "2020-01-01T00:00:00Z", "Oldest")], &[], &merged);
        assert_eq!(chosen.action, AutoAction::Idle);
        assert_eq!(chosen.skipped, vec![Skip::Closed { number: 9, pr: 8 }]);
    }

    #[test]
    fn idle_checks_keep_waiting() {
        let blocked = vec![open_pr(3, "", "", vec![9, 2, 1])];
        let polls = [
            choose(&[], &[], &[]),
            choose(&[], &[], &[]),
            choose(&sample(), &[], &blocked),
        ];
        assert!(idle_waits(&polls));
        assert!(idle_waits(&polls[..2]), "an empty list keeps waiting");
    }

    #[test]
    fn collaborators_are_picked_and_outsiders_are_skipped() {
        let mut outsider = issue(4, "2020-01-01T00:00:00Z", "Outside");
        outsider.author = "pat".into();
        outsider.association = "NONE".into();
        let mut contributor = issue(5, "2020-02-01T00:00:00Z", "Contrib");
        contributor.association = "CONTRIBUTOR".into();
        let owner = issue(6, "2020-03-01T00:00:00Z", "Owner");
        let rows = vec![outsider, contributor.clone(), owner];
        let chosen = poll(&PollIn {
            issues: &rows,
            failed: &[],
            prs: &[],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-01-01T00:00:00Z",
            busy: &[],
        });
        assert_eq!(chosen.action, AutoAction::Run(6));
        assert_eq!(chosen.skipped.len(), 2);
        assert!(matches!(
            &chosen.skipped[0],
            Skip::Untrusted { number: 4, association, .. } if association == "NONE"
        ));
        let widened = poll(&PollIn {
            issues: &rows,
            failed: &[],
            prs: &[],
            trust: CommentTrust::Contributors,
            created: &[],
            now: "2026-01-01T00:00:00Z",
            busy: &[],
        });
        assert_eq!(widened.action, AutoAction::Run(5));
        let everyone = poll(&PollIn {
            issues: &[contributor],
            failed: &[],
            prs: &[],
            trust: CommentTrust::All,
            created: &[],
            now: "2026-01-01T00:00:00Z",
            busy: &[],
        });
        assert_eq!(everyone.action, AutoAction::Run(5));
    }

    #[test]
    fn issues_the_user_files_after_auto_starts_are_taken() {
        // The user and the agent share one gh account, so author and time do not skip.
        let mut fresh = issue(12, "2026-02-01T00:00:00Z", "Filed while auto runs");
        fresh.author = "ada".into();
        let agent_made = issue(3, "2020-01-01T00:00:00Z", "Opened by gh issue create");
        let chosen = poll(&PollIn {
            issues: &[agent_made, fresh],
            failed: &[],
            prs: &[],
            trust: CommentTrust::Collaborators,
            created: &[3],
            now: "2026-02-02T00:00:00Z",
            busy: &[],
        });
        assert_eq!(chosen.action, AutoAction::Run(12));
        assert_eq!(chosen.skipped, vec![Skip::AgentCreated { number: 3 }]);
    }

    #[test]
    fn a_transient_failure_retries_after_the_deadline_or_a_new_head() {
        let row = issue(9, "2020-01-01T00:00:00Z", "Oldest");
        let hold = AutoFailure {
            number: 9,
            reason: "gh pr merge failed: HTTP 502".into(),
            transient: true,
            retry_at: "2026-02-01T00:00:00Z".into(),
            head: "aaa".into(),
            attempts: 1,
        };
        let open = PrLink {
            number: 40,
            head_ref: workdir::issue_branch(9, "Oldest", &[]),
            head_oid: "aaa".into(),
            body: String::new(),
            closing: vec![],
            state: PrLife::Open,
        };
        let early = poll(&PollIn {
            issues: &[row.clone()],
            failed: &[hold.clone()],
            prs: &[open.clone()],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-01-15T00:00:00Z",
            busy: &[],
        });
        assert_eq!(early.action, AutoAction::Idle);
        assert!(matches!(early.skipped.first(), Some(Skip::Backoff { .. })));
        let due = poll(&PollIn {
            issues: &[row.clone()],
            failed: &[hold.clone()],
            prs: &[open.clone()],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-02-01T00:00:00Z",
            busy: &[],
        });
        assert_eq!(due.action, AutoAction::Run(9));
        let mut moved = open.clone();
        moved.head_oid = "bbb".into();
        let changed = poll(&PollIn {
            issues: &[row.clone()],
            failed: &[hold],
            prs: &[moved],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-01-15T00:00:00Z",
            busy: &[],
        });
        assert_eq!(changed.action, AutoAction::Run(9));
        let closed = PrLink {
            state: PrLife::Closed,
            ..open
        };
        let held = AutoFailure {
            number: 9,
            reason: "HTTP 502".into(),
            transient: true,
            retry_at: "2020-01-01T00:00:00Z".into(),
            head: "aaa".into(),
            attempts: 1,
        };
        let done = poll(&PollIn {
            issues: &[row],
            failed: &[held],
            prs: &[closed],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-03-01T00:00:00Z",
            busy: &[],
        });
        assert_eq!(done.action, AutoAction::Idle);
        assert_eq!(done.skipped, vec![Skip::Closed { number: 9, pr: 40 }]);
    }

    #[test]
    fn a_busy_issue_is_skipped_for_this_round_only() {
        let chosen = poll(&PollIn {
            issues: &sample(),
            failed: &[],
            prs: &[],
            trust: CommentTrust::Collaborators,
            created: &[],
            now: "2026-01-01T00:00:00Z",
            busy: &[9],
        });
        assert_eq!(chosen.action, AutoAction::Run(2));
        assert_eq!(chosen.skipped, vec![Skip::Busy { number: 9 }]);
    }

    #[test]
    fn parses_issue_and_pr_json() {
        let issues = parse_issues(
            r#"[{"number":4,"title":"Crash","labels":[{"name":"bug"}],"createdAt":"2024-05-01T00:00:00Z","author":{"login":"ada","is_bot":false},"authorAssociation":"OWNER"},{"number":0}]"#,
        )
        .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 4);
        assert_eq!(issues[0].labels, vec!["bug".to_string()]);
        assert_eq!(issues[0].created_at, "2024-05-01T00:00:00Z");
        assert_eq!(issues[0].author, "ada");
        assert_eq!(issues[0].association, "OWNER");
        assert!(!issues[0].is_bot);
        let prs = parse_pr_links(
            r#"[{"number":8,"headRefName":"fix/4-crash","headRefOid":"abc","body":"Closes #4","state":"OPEN","closingIssuesReferences":[{"number":4}]}]"#,
        )
        .unwrap();
        assert_eq!(prs[0].closing, vec![4]);
        assert_eq!(prs[0].head_ref, "fix/4-crash");
        assert_eq!(prs[0].state, PrLife::Open);
    }

    #[test]
    fn graphql_pages_keep_a_cursor_past_the_first_hundred() {
        let mut nodes = Vec::new();
        for number in 1..=100 {
            nodes.push(format!(
                r#"{{"number":{number},"title":"t{number}","createdAt":"2020-01-01T00:00:00Z","authorAssociation":"OWNER","author":{{"__typename":"User","login":"ada"}},"labels":{{"nodes":[]}}}}"#
            ));
        }
        let page = format!(
            r#"{{"data":{{"repository":{{"issues":{{"pageInfo":{{"hasNextPage":true,"endCursor":"PAGE2"}},"nodes":[{}]}}}}}}}}"#,
            nodes.join(",")
        );
        let first = parse_issue_page(&page).unwrap();
        assert_eq!(first.rows.len(), 100);
        assert_eq!(
            next_cursor(first.has_next, first.cursor),
            Some("PAGE2".into())
        );
        let second = parse_issue_page(
            r#"{"data":{"repository":{"issues":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"number":101,"title":"last","createdAt":"2021-01-01T00:00:00Z","authorAssociation":"MEMBER","author":{"__typename":"Bot","login":"ghost"},"labels":{"nodes":[{"name":"bug"}]}}]}}}}"#,
        )
        .unwrap();
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0].number, 101);
        assert!(second.rows[0].is_bot);
        assert_eq!(second.rows[0].labels, vec!["bug".to_string()]);
        assert_eq!(next_cursor(second.has_next, second.cursor.clone()), None);
        assert!(first.rows.len() + second.rows.len() > 100);
        let err = parse_issue_page(r#"{"errors":[{"message":"HTTP 502"}]}"#).unwrap_err();
        assert!(err.contains("502"));
    }

    #[test]
    fn rfc3339_round_trips_known_days() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_609_459_200), "2021-01-01T00:00:00Z");
        assert!(is_transient("gh issue list failed: HTTP 502"));
        assert!(is_transient("connection reset"));
        assert!(!is_transient("no pull request appeared"));
    }
}
