/// What `gh auth status` and `gh repo view` told ghost, without running them.
#[derive(Debug, Clone)]
pub struct AuthProbe {
    pub found: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct RepoProbe {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightFailure {
    GhMissing,
    NotAuthenticated,
    RepoInaccessible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightOk {
    pub login: Option<String>,
}

/// gh missing and a missing login stop the process. A repo probe failure does not:
/// ghost hands that step to the agent.
pub fn is_fatal(kind: &PreflightFailure) -> bool {
    matches!(
        kind,
        PreflightFailure::GhMissing | PreflightFailure::NotAuthenticated
    )
}

pub fn evaluate(
    auth: &AuthProbe,
    repo: Option<&RepoProbe>,
    _slug: &str,
) -> Result<PreflightOk, PreflightFailure> {
    if !auth.found {
        return Err(PreflightFailure::GhMissing);
    }
    let combined = format!("{}\n{}", auth.stdout, auth.stderr);
    let logged_in = auth.exit_code == Some(0) && combined.contains("Logged in");
    if !logged_in {
        return Err(PreflightFailure::NotAuthenticated);
    }
    match repo {
        None => Err(PreflightFailure::RepoInaccessible),
        Some(repo) if repo.exit_code == 0 && !repo.stdout.trim().is_empty() => Ok(PreflightOk {
            login: parse_login(&combined),
        }),
        Some(_) => Err(PreflightFailure::RepoInaccessible),
    }
}

pub fn parse_login(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(idx) = line.find("account ") else {
            continue;
        };
        let rest = &line[idx + "account ".len()..];
        let name = rest.split_whitespace().next().unwrap_or("");
        let name = name.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Some(name.to_string());
        }
    }
    None
}

pub fn failure_message(
    kind: &PreflightFailure,
    auth: &AuthProbe,
    repo: Option<&RepoProbe>,
    slug: &str,
    lang: crate::lang::Lang,
) -> String {
    let detail_auth = first_line(&auth.stderr, &auth.stdout);
    let detail_repo = repo
        .map(|r| first_line(&r.stderr, &r.stdout))
        .unwrap_or_else(|| {
            lang.say(
                "リポジトリを照会できませんでした",
                "could not query the repository",
            )
        });
    match (kind, lang) {
        (PreflightFailure::GhMissing, crate::lang::Lang::Ja) => "\
`gh` がインストールされていません（PATH にありません）。

ghost は GitHub の操作をすべて GitHub CLI で行います。

直し方:
  GitHub CLI を入れる: https://cli.github.com/
  その後: gh auth login"
            .to_string(),
        (PreflightFailure::GhMissing, crate::lang::Lang::En) => "\
`gh` is not installed (not on PATH).

ghost uses the GitHub CLI for every GitHub operation.

Fix:
  install GitHub CLI: https://cli.github.com/
  then: gh auth login"
            .to_string(),
        (PreflightFailure::NotAuthenticated, crate::lang::Lang::Ja) => format!(
            "`gh` は入っていますが、認証されていません。\n\n{detail_auth}\n\n直し方:\n  gh auth login"
        ),
        (PreflightFailure::NotAuthenticated, crate::lang::Lang::En) => format!(
            "`gh` is installed but not authenticated.\n\n{detail_auth}\n\nFix:\n  gh auth login"
        ),
        (PreflightFailure::RepoInaccessible, crate::lang::Lang::Ja) => format!(
            "{slug} にアクセスできません。\n\n{detail_repo}\n\n直し方:\n  gh auth login\n  gh repo view {slug}\n  このアカウントが {slug} を読めるか確認する"
        ),
        (PreflightFailure::RepoInaccessible, crate::lang::Lang::En) => format!(
            "Could not access {slug}.\n\n{detail_repo}\n\nFix:\n  gh auth login\n  gh repo view {slug}\n  confirm this account can read {slug}"
        ),
    }
}

fn first_line(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    // Never surface a token if gh printed one.
    let line = line.replace(|c: char| c == '\t', " ");
    if line.to_ascii_lowercase().contains("token") {
        "gh rejected the credentials".to_string()
    } else if line.is_empty() {
        "gh returned no details".to_string()
    } else {
        line.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(found: bool, code: Option<i32>, stdout: &str, stderr: &str) -> AuthProbe {
        AuthProbe {
            found,
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn missing_gh() {
        let probe = auth(false, None, "", "");
        let err = evaluate(&probe, None, "a/b").unwrap_err();
        assert_eq!(err, PreflightFailure::GhMissing);
        let msg = failure_message(&err, &probe, None, "a/b", crate::lang::Lang::En);
        assert!(msg.contains("not installed"));
        assert!(msg.contains("gh auth login"));
        assert!(msg.contains("cli.github.com"));
    }

    #[test]
    fn not_authenticated() {
        let probe = auth(
            true,
            Some(1),
            "",
            "You are not logged into any GitHub hosts. To log in, run: gh auth login",
        );
        let err = evaluate(&probe, None, "a/b").unwrap_err();
        assert_eq!(err, PreflightFailure::NotAuthenticated);
        let msg = failure_message(&err, &probe, None, "a/b", crate::lang::Lang::En);
        assert!(msg.contains("gh auth login"), "{msg}");
    }

    #[test]
    fn exit_zero_without_logged_in_line_is_unauthenticated() {
        let probe = auth(true, Some(0), "github.com\n", "");
        assert_eq!(
            evaluate(&probe, None, "a/b").unwrap_err(),
            PreflightFailure::NotAuthenticated
        );
    }

    #[test]
    fn repo_denied() {
        let probe = auth(
            true,
            Some(0),
            "github.com\n  ✓ Logged in to github.com account octocat (/home/x)\n",
            "",
        );
        let repo = RepoProbe {
            exit_code: 1,
            stdout: "".into(),
            stderr: "GraphQL: Could not resolve to a Repository with the name 'a/b'.".into(),
        };
        let err = evaluate(&probe, Some(&repo), "a/b").unwrap_err();
        assert_eq!(err, PreflightFailure::RepoInaccessible);
        let msg = failure_message(&err, &probe, Some(&repo), "a/b", crate::lang::Lang::En);
        assert!(msg.contains("gh auth login"));
        assert!(msg.contains("gh repo view a/b"));
        assert!(msg.contains("Could not resolve"));
    }

    #[test]
    fn only_missing_gh_and_auth_are_fatal() {
        assert!(is_fatal(&PreflightFailure::GhMissing));
        assert!(is_fatal(&PreflightFailure::NotAuthenticated));
        assert!(!is_fatal(&PreflightFailure::RepoInaccessible));
    }

    #[test]
    fn success_parses_login() {
        let stdout = "\
github.com
  ✓ Logged in to github.com account cursor (/home/ubuntu/.config/gh/hosts.yml)
  - Active account: true
  - Token: ghs_secret
";
        let probe = auth(true, Some(0), stdout, "");
        let repo = RepoProbe {
            exit_code: 0,
            stdout: "{\"nameWithOwner\":\"cympfh/ghost\"}\n".into(),
            stderr: "".into(),
        };
        let ok = evaluate(&probe, Some(&repo), "cympfh/ghost").unwrap();
        assert_eq!(ok.login.as_deref(), Some("cursor"));
        assert_eq!(parse_login(stdout).as_deref(), Some("cursor"));
    }
}
