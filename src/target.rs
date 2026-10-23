/// A GitHub repository, pull request, or issue addressed by a ghost command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhTarget {
    Repo {
        owner: String,
        repo: String,
    },
    Pull {
        owner: String,
        repo: String,
        number: u64,
    },
    Issue {
        owner: String,
        repo: String,
        number: u64,
    },
    /// `github.com/OWNER/REPO/issues` with no number. Open issues only.
    Issues {
        owner: String,
        repo: String,
    },
    /// `github.com/OWNER/REPO/pulls` with no number. Open pull requests only.
    Pulls {
        owner: String,
        repo: String,
    },
}

impl GhTarget {
    pub fn owner(&self) -> &str {
        match self {
            Self::Repo { owner, .. }
            | Self::Pull { owner, .. }
            | Self::Issue { owner, .. }
            | Self::Issues { owner, .. }
            | Self::Pulls { owner, .. } => owner,
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            Self::Repo { repo, .. }
            | Self::Pull { repo, .. }
            | Self::Issue { repo, .. }
            | Self::Issues { repo, .. }
            | Self::Pulls { repo, .. } => repo,
        }
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner(), self.repo())
    }

    /// Canonical display form used in the TUI header and state paths.
    pub fn display_url(&self) -> String {
        match self {
            Self::Repo { owner, repo } => format!("github.com/{owner}/{repo}"),
            Self::Pull {
                owner,
                repo,
                number,
            } => format!("github.com/{owner}/{repo}/pull/{number}"),
            Self::Issue {
                owner,
                repo,
                number,
            } => format!("github.com/{owner}/{repo}/issues/{number}"),
            Self::Issues { owner, repo } => format!("github.com/{owner}/{repo}/issues"),
            Self::Pulls { owner, repo } => format!("github.com/{owner}/{repo}/pulls"),
        }
    }
}

pub fn parse_target(input: &str) -> Result<GhTarget, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("empty GitHub URL".into());
    }
    let no_frag = raw.split('#').next().unwrap_or(raw);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let s = no_query.trim().trim_end_matches('/');
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();

    let (owner, repo, extra): (&str, &str, &[&str]) = if parts.first().copied()
        == Some("github.com")
    {
        if parts.len() < 3 {
            return Err(format!("expected github.com/OWNER/REPO, got `{input}`"));
        }
        (parts[1], strip_git(parts[2]), &parts[3..])
    } else if parts.len() >= 2 && !parts[0].contains('.') {
        (parts[0], strip_git(parts[1]), &parts[2..])
    } else {
        return Err(format!(
                "not a github.com URL: `{input}` (expected github.com/OWNER/REPO[/pull/N|/pulls|/issues/N|/issues])"
            ));
    };

    validate_name("owner", owner)?;
    validate_name("repo", repo)?;
    let owner = owner.to_string();
    let repo = repo.to_string();

    match extra {
        [] => Ok(GhTarget::Repo { owner, repo }),
        ["pull", n] => Ok(GhTarget::Pull {
            owner,
            repo,
            number: parse_number(n)?,
        }),
        ["issues", n] => Ok(GhTarget::Issue {
            owner,
            repo,
            number: parse_number(n)?,
        }),
        ["issues"] => Ok(GhTarget::Issues { owner, repo }),
        ["pulls"] => Ok(GhTarget::Pulls { owner, repo }),
        ["pr", rest @ ..] => Err(suggest_pull("pr", &owner, &repo, rest)),
        ["pulls", rest @ ..] => Err(suggest_pull("pulls", &owner, &repo, rest)),
        ["issue", rest @ ..] => Err(suggest_issues(&owner, &repo, rest)),
        _ => Err(format!(
            "unsupported GitHub URL `{input}` (use github.com/OWNER/REPO, /pull/N, /pulls, /issues/N, or /issues)"
        )),
    }
}

/// `/pr/N` and `/pulls/N` are not pull-request URLs. Point at `/pull/N`.
/// Bare `/pulls` is the open-list form and is handled before this.
fn suggest_pull(rejected: &str, owner: &str, repo: &str, rest: &[&str]) -> String {
    let number = rest.first().copied().unwrap_or("N");
    let shown = if rest.len() == 1 && number.chars().all(|c| c.is_ascii_digit()) && number != "0" {
        number
    } else {
        "N"
    };
    format!(
        "`/{rejected}/N` is not a GitHub URL. Use /pull/{shown}: github.com/{owner}/{repo}/pull/{shown}"
    )
}

/// `/issue` and `/issue/N` are not the form ghost accepts. Point at `/issues`.
fn suggest_issues(owner: &str, repo: &str, rest: &[&str]) -> String {
    match rest {
        [] => format!(
            "`/issue` is not a GitHub URL. Use /issues: github.com/{owner}/{repo}/issues"
        ),
        [n] if n.chars().all(|c| c.is_ascii_digit()) && *n != "0" => format!(
            "`/issue/{n}` is not a GitHub URL. Use /issues/{n}: github.com/{owner}/{repo}/issues/{n}"
        ),
        _ => format!(
            "`/issue/N` is not a GitHub URL. Use /issues/N: github.com/{owner}/{repo}/issues/N"
        ),
    }
}

fn strip_git(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
}

fn parse_number(raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("bad issue or PR number `{raw}`"));
    }
    let n: u64 = raw
        .parse()
        .map_err(|_| format!("bad issue or PR number `{raw}`"))?;
    if n == 0 {
        return Err("issue or PR number must be at least 1".into());
    }
    Ok(n)
}

fn validate_name(what: &str, name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("bad GitHub {what} `{name}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(owner: &str, repo: &str, n: u64) -> GhTarget {
        GhTarget::Pull {
            owner: owner.into(),
            repo: repo.into(),
            number: n,
        }
    }

    #[test]
    fn parses_pr_issue_and_repo() {
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/pull/3").unwrap(),
            pull("cympfh", "xxx", 3)
        );
        assert_eq!(
            parse_target("http://www.github.com/cympfh/xxx/pull/3/").unwrap(),
            pull("cympfh", "xxx", 3)
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/pull/3")
                .unwrap()
                .display_url(),
            "github.com/cympfh/xxx/pull/3"
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/issues/12?tab=1").unwrap(),
            GhTarget::Issue {
                owner: "cympfh".into(),
                repo: "xxx".into(),
                number: 12,
            }
        );
        assert_eq!(
            parse_target("github.com/cympfh/xxx/issues/12")
                .unwrap()
                .display_url(),
            "github.com/cympfh/xxx/issues/12"
        );
        assert_eq!(
            parse_target("github.com/cympfh/xxx").unwrap(),
            GhTarget::Repo {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx.git").unwrap(),
            GhTarget::Repo {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
        assert_eq!(
            parse_target("cympfh/xxx/pull/9").unwrap(),
            pull("cympfh", "xxx", 9)
        );
        assert_eq!(
            parse_target("github.com/cympfh/xxx/issues").unwrap(),
            GhTarget::Issues {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/issues/").unwrap(),
            GhTarget::Issues {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
        assert_eq!(
            parse_target("github.com/cympfh/xxx/issues")
                .unwrap()
                .display_url(),
            "github.com/cympfh/xxx/issues"
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/pulls").unwrap(),
            GhTarget::Pulls {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
        assert_eq!(
            parse_target("http://www.github.com/cympfh/xxx/pulls/?q=is%3Aopen")
                .unwrap()
                .display_url(),
            "github.com/cympfh/xxx/pulls"
        );
        assert_eq!(
            parse_target("cympfh/xxx/pulls").unwrap(),
            GhTarget::Pulls {
                owner: "cympfh".into(),
                repo: "xxx".into(),
            }
        );
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_target("").is_err());
        assert!(parse_target("github.com/onlyowner").is_err());
        assert!(parse_target("https://gitlab.com/a/b").is_err());
        assert!(parse_target("github.com/a/b/pull/x").is_err());
        assert!(parse_target("github.com/a/b/pull/0").is_err());
        assert!(parse_target("github.com/a/b/pull/3/files").is_err());
        assert!(parse_target("github.com/a/b/commit/abc").is_err());
        let pr = parse_target("github.com/a/b/pr/3").unwrap_err();
        assert!(pr.contains("/pull/3"), "{pr}");
        assert!(pr.contains("github.com/a/b/pull/3"), "{pr}");
        let pulls = parse_target("http://www.github.com/a/b/pulls/3/").unwrap_err();
        assert!(pulls.contains("/pull/3"), "{pulls}");
        assert!(pulls.contains("github.com/a/b/pull/3"), "{pulls}");
        let bare_pulls = parse_target("github.com/a/b/pulls").unwrap();
        assert_eq!(bare_pulls.display_url(), "github.com/a/b/pulls");
        assert!(!parse_target("github.com/a/b/pulls/0")
            .unwrap_err()
            .contains("at least 1"));
        let issue = parse_target("github.com/a/b/issue/12").unwrap_err();
        assert!(issue.contains("/issues/12"), "{issue}");
        assert!(issue.contains("github.com/a/b/issues/12"), "{issue}");
        let list = parse_target("https://github.com/a/b/issue/").unwrap_err();
        assert!(list.contains("/issues"), "{list}");
        assert!(list.contains("github.com/a/b/issues"), "{list}");
        let bare = parse_target("cympfh/xxx/pr/9").unwrap_err();
        assert!(bare.contains("/pull/"), "{bare}");
        let zero = parse_target("github.com/a/b/pr/0").unwrap_err();
        assert!(zero.contains("/pull/N"), "{zero}");
        assert!(!zero.contains("at least 1"), "{zero}");
        let junk = parse_target("github.com/a/b/issue/x").unwrap_err();
        assert!(junk.contains("/issues/N"), "{junk}");
        assert!(junk.contains("github.com/a/b/issues/N"), "{junk}");
    }
}
