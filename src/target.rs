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
}

impl GhTarget {
    pub fn owner(&self) -> &str {
        match self {
            Self::Repo { owner, .. }
            | Self::Pull { owner, .. }
            | Self::Issue { owner, .. }
            | Self::Issues { owner, .. } => owner,
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            Self::Repo { repo, .. }
            | Self::Pull { repo, .. }
            | Self::Issue { repo, .. }
            | Self::Issues { repo, .. } => repo,
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
            } => format!("github.com/{owner}/{repo}/pr/{number}"),
            Self::Issue {
                owner,
                repo,
                number,
            } => format!("github.com/{owner}/{repo}/issue/{number}"),
            Self::Issues { owner, repo } => format!("github.com/{owner}/{repo}/issues"),
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
                "not a github.com URL: `{input}` (expected github.com/OWNER/REPO[/pr/N|/issue/N|/issues])"
            ));
    };

    validate_name("owner", owner)?;
    validate_name("repo", repo)?;
    let owner = owner.to_string();
    let repo = repo.to_string();

    match extra {
        [] => Ok(GhTarget::Repo { owner, repo }),
        [kind, n] if matches!(*kind, "pr" | "pull" | "pulls") => Ok(GhTarget::Pull {
            owner,
            repo,
            number: parse_number(n)?,
        }),
        [kind, n] if matches!(*kind, "issue" | "issues") => Ok(GhTarget::Issue {
            owner,
            repo,
            number: parse_number(n)?,
        }),
        [kind] if matches!(*kind, "issue" | "issues") => Ok(GhTarget::Issues { owner, repo }),
        _ => Err(format!(
            "unsupported GitHub URL `{input}` (use /pr/N, /pull/N, /issue/N, /issues, or the repo root)"
        )),
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
            parse_target("github.com/cympfh/xxx/pr/3").unwrap(),
            pull("cympfh", "xxx", 3)
        );
        assert_eq!(
            parse_target("https://github.com/cympfh/xxx/pull/3").unwrap(),
            pull("cympfh", "xxx", 3)
        );
        assert_eq!(
            parse_target("http://www.github.com/cympfh/xxx/pulls/3/").unwrap(),
            pull("cympfh", "xxx", 3)
        );
        assert_eq!(
            parse_target("github.com/cympfh/xxx/issue/12").unwrap(),
            GhTarget::Issue {
                owner: "cympfh".into(),
                repo: "xxx".into(),
                number: 12,
            }
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
            parse_target("cympfh/xxx/pr/9").unwrap(),
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
            parse_target("https://github.com/cympfh/xxx/issue/").unwrap(),
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
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_target("").is_err());
        assert!(parse_target("github.com/onlyowner").is_err());
        assert!(parse_target("https://gitlab.com/a/b").is_err());
        assert!(parse_target("github.com/a/b/pr/x").is_err());
        assert!(parse_target("github.com/a/b/pr/0").is_err());
        assert!(parse_target("github.com/a/b/pr/3/files").is_err());
        assert!(parse_target("github.com/a/b/commit/abc").is_err());
    }
}
