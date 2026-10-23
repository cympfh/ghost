//! Choose the next open issue for `--auto`.
//!
//! An empty result waits for the next check. It does not end the process.

use serde_json::Value;

use crate::watch::body_closes_issue;
use crate::workdir;

/// One open issue from `gh issue list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub created_at: String,
}

/// An open pull request that may already cover an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLink {
    pub number: u64,
    pub head_ref: String,
    pub body: String,
    pub closing: Vec<u64>,
}

/// Why an older issue was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    Failed { number: u64, reason: String },
    OpenPr { number: u64, pr: u64 },
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

/// Oldest eligible issue. Dated issues come before ones with an empty timestamp.
pub fn poll(issues: &[IssueRow], failed: &[(u64, String)], prs: &[PrLink]) -> Poll {
    let mut rows: Vec<&IssueRow> = issues.iter().filter(|row| row.number > 0).collect();
    rows.sort_by_key(|row| oldest_key(row));
    let mut skipped = Vec::new();
    for row in rows {
        if let Some(reason) = failed_reason(failed, row.number) {
            skipped.push(Skip::Failed {
                number: row.number,
                reason,
            });
            continue;
        }
        if let Some(pr) = open_pr_for(row, prs) {
            skipped.push(Skip::OpenPr {
                number: row.number,
                pr,
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

fn oldest_key(row: &IssueRow) -> (bool, String, u64) {
    (
        row.created_at.is_empty(),
        row.created_at.clone(),
        row.number,
    )
}

fn failed_reason(failed: &[(u64, String)], number: u64) -> Option<String> {
    failed
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, reason)| reason.clone())
}

fn open_pr_for(issue: &IssueRow, prs: &[PrLink]) -> Option<u64> {
    let labels: Vec<&str> = issue.labels.iter().map(String::as_str).collect();
    let branch = workdir::issue_branch(issue.number, &issue.title, &labels);
    prs.iter()
        .filter(|pr| {
            pr.head_ref == branch
                || pr.closing.contains(&issue.number)
                || body_closes_issue(&pr.body, issue.number)
        })
        .map(|pr| pr.number)
        .min()
}

pub fn parse_issues(json: &str) -> Result<Vec<IssueRow>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad issue list: {e}"))?;
    let mut rows = Vec::new();
    for v in arr {
        let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        rows.push(IssueRow {
            number,
            title: text(&v, "title"),
            labels: names(v.get("labels")),
            created_at: text(&v, "createdAt"),
        });
    }
    Ok(rows)
}

pub fn parse_pr_links(json: &str) -> Result<Vec<PrLink>, String> {
    let arr: Vec<Value> = serde_json::from_str(json).map_err(|e| format!("bad pr list: {e}"))?;
    let mut rows = Vec::new();
    for v in arr {
        let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        if number == 0 {
            continue;
        }
        rows.push(PrLink {
            number,
            head_ref: text(&v, "headRefName"),
            body: text(&v, "body"),
            closing: closing_numbers(v.get("closingIssuesReferences")),
        });
    }
    Ok(rows)
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

fn names(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: u64, created_at: &str, title: &str) -> IssueRow {
        IssueRow {
            number,
            title: title.into(),
            labels: Vec::new(),
            created_at: created_at.into(),
        }
    }

    fn sample() -> Vec<IssueRow> {
        vec![
            issue(2, "2020-03-01T00:00:00Z", "Later"),
            issue(9, "2020-01-01T00:00:00Z", "Oldest"),
            issue(1, "", "Undated"),
        ]
    }

    #[test]
    fn oldest_dated_issue_is_first_and_only_one_is_chosen() {
        let poll = poll(&sample(), &[], &[]);
        assert_eq!(poll.action, AutoAction::Run(9));
        assert!(poll.skipped.is_empty());
    }

    #[test]
    fn skips_failures_then_open_prs_and_leaves_the_rest() {
        let branch = workdir::issue_branch(2, "Later", &[]);
        let prs = vec![
            PrLink {
                number: 40,
                head_ref: "unrelated".into(),
                body: "see #2".into(),
                closing: vec![],
            },
            PrLink {
                number: 41,
                head_ref: branch,
                body: String::new(),
                closing: vec![],
            },
        ];
        let failed = vec![(9, "no pull request".into())];
        let poll = poll(&sample(), &failed, &prs);
        assert_eq!(poll.action, AutoAction::Run(1));
        assert_eq!(
            poll.skipped,
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
        let prs = vec![PrLink {
            number: 8,
            head_ref: "feature".into(),
            body: "Closes #9".into(),
            closing: vec![1],
        }];
        let chosen = poll(&sample(), &[], &prs);
        assert_eq!(chosen.action, AutoAction::Run(2));
        assert_eq!(chosen.skipped, vec![Skip::OpenPr { number: 9, pr: 8 }]);
        let only_undated = poll(&[issue(1, "", "Undated")], &[], &prs);
        assert_eq!(only_undated.action, AutoAction::Idle);
        assert_eq!(
            only_undated.skipped,
            vec![Skip::OpenPr { number: 1, pr: 8 }]
        );
    }

    #[test]
    fn a_pull_request_that_is_not_open_does_not_skip() {
        let poll = poll(&sample(), &[], &[]);
        assert_eq!(poll.action, AutoAction::Run(9));
    }

    #[test]
    fn idle_checks_keep_waiting() {
        let blocked = vec![PrLink {
            number: 3,
            head_ref: String::new(),
            body: String::new(),
            closing: vec![9, 2, 1],
        }];
        let polls = [
            poll(&[], &[], &[]),
            poll(&[], &[], &[]),
            poll(&sample(), &[], &blocked),
        ];
        assert!(idle_waits(&polls));
        assert!(idle_waits(&polls[..2]), "an empty list keeps waiting");
    }

    #[test]
    fn parses_issue_and_pr_json() {
        let issues = parse_issues(
            r#"[{"number":4,"title":"Crash","labels":[{"name":"bug"}],"createdAt":"2024-05-01T00:00:00Z"},{"number":0}]"#,
        )
        .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 4);
        assert_eq!(issues[0].labels, vec!["bug".to_string()]);
        assert_eq!(issues[0].created_at, "2024-05-01T00:00:00Z");
        let prs = parse_pr_links(
            r#"[{"number":8,"headRefName":"fix/4-crash","body":"Closes #4","closingIssuesReferences":[{"number":4}]}]"#,
        )
        .unwrap();
        assert_eq!(prs[0].closing, vec![4]);
        assert_eq!(prs[0].head_ref, "fix/4-crash");
    }
}
