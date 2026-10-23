use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};

use crate::agent::AgentKind;
use crate::demo::{self, DemoScene};
use crate::engine::{self, RunMode, RunSpec};
use crate::interval::parse_interval;
use crate::lang::Lang;
use crate::target::{parse_target, GhTarget};
use crate::watch::CommentTrust;

#[derive(Parser, Debug)]
#[command(
    name = "ghost",
    version,
    about = "Babysit a GitHub PR or issue with one local coding agent until it is merge-ready",
    after_help = "\
Examples:
  ghost github.com/owner/repo
  ghost github.com/owner/repo/pr/3 -r
  ghost github.com/owner/repo/pr/3 -c -i 20s
  ghost github.com/owner/repo/issue/7 -y -l ja
  ghost github.com/owner/repo/issues --auto
  ghost -d review

GitHub access is only through the `gh` CLI. One agent session is started for the
run; later updates resume it (claude --resume, codex exec resume, grok --resume).
The default poll interval is 20s (`-i`). `-l` / `--lang` defaults to ja.
`-k` / `--ssh-key` selects an ssh identity (or set GHOST_SSH_KEY).
`-h` prints help and `-V` prints the version.
Launch always starts the agent for a situation check, even when nothing is new.
Later polls with nothing new do not call the agent. `i` always delivers text to the agent.
`--merge-yes` (no short flag) merges in --code, issue, or --auto mode after a re-check. `-y` does not merge.
`--auto` is only valid with /issues. It takes the oldest open issue on each check and keeps waiting when none are left."
)]
struct Cli {
    /// github.com/OWNER/REPO, .../pr/N, .../issue/N, or .../issues
    target: Option<String>,

    /// Review the pull request when its head SHA changes.
    #[arg(long, short = 'r', conflicts_with = "code")]
    review: bool,

    /// Update the pull request when CI fails or new review comments appear.
    #[arg(long, short = 'c', conflicts_with = "review")]
    code: bool,

    /// With /issues, take the oldest open issue on each check. No picker.
    #[arg(long)]
    auto: bool,

    /// How often to poll `gh`. Default 20s. First check is immediate.
    /// Examples: 20s, 30s, 2m, 1h30m, or a bare number of seconds.
    #[arg(long, short = 'i', default_value = "20s")]
    interval: String,

    /// Coding agent. Default: first of codex, claude, grok on PATH.
    #[arg(long, short = 'a', value_enum)]
    agent: Option<AgentKind>,

    /// Accept the issue-mode prompt to continue as --code, and the review exit prompt.
    /// Does not merge.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Merge when --code, issue, or --auto is merge-ready, after a fresh re-check.
    /// No short flag. Does not apply to --review, the picker, or ask-without---code.
    #[arg(long)]
    merge_yes: bool,

    /// `gh pr merge` method. Default is squash (`--squash`).
    #[arg(long, value_enum)]
    merge_method: Option<MergeMethod>,

    /// Stop after this many agent turns so --review and --code cannot ping-pong forever.
    #[arg(long, short = 'n', default_value_t = 20)]
    max_iterations: u32,

    /// Draw a scripted screen (picker, review, code, confirm, preflight).
    #[arg(long, short = 'd', value_enum)]
    demo: Option<DemoScene>,

    /// Language for prompts and user-facing text. Default: ja.
    #[arg(long, short = 'l', value_enum, default_value_t = Lang::Ja)]
    lang: Lang,

    /// SSH private key for git and the inner agent. Default: ssh config and agent.
    /// `GHOST_SSH_KEY` is used when this flag is omitted.
    #[arg(long, short = 'k')]
    ssh_key: Option<String>,

    /// Who may wake --code, including review comments during an issue and during --code.
    /// Default: all.
    #[arg(long, short = 't', value_enum)]
    trust_comments: Option<CommentTrust>,

    /// Which issue authors --auto may run.
    /// Default: collaborators (OWNER, MEMBER, COLLABORATOR).
    #[arg(long, value_enum)]
    trust_issues: Option<CommentTrust>,
}

pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let header = invocation();
    let lang = lang_from_env(cli.lang);
    let ssh_key = cli
        .ssh_key
        .clone()
        .or_else(|| std::env::var("GHOST_SSH_KEY").ok())
        .filter(|s| !s.trim().is_empty());
    match resolve(
        cli.target.as_deref(),
        cli.review,
        cli.code,
        cli.auto,
        &cli.interval,
        cli.agent,
        cli.yes,
        cli.max_iterations,
        cli.demo,
        header,
        lang,
        ssh_key,
        cli.trust_comments,
        cli.trust_issues,
        cli.merge_yes,
        cli.merge_method
            .map(MergeMethod::gh_flag)
            .map(str::to_string),
    ) {
        Ok(Action::Demo { scene, header }) => demo::run(scene, header),
        Ok(Action::Run(spec)) => engine::run(spec),
        Err(err) => {
            eprintln!("ghost: {err}");
            eprintln!("try `ghost --help`");
            Ok(1)
        }
    }
}

enum Action {
    Demo { scene: DemoScene, header: String },
    Run(RunSpec),
}

#[allow(clippy::too_many_arguments)]
fn resolve(
    target: Option<&str>,
    review: bool,
    code: bool,
    auto: bool,
    interval: &str,
    agent: Option<AgentKind>,
    yes: bool,
    max_iterations: u32,
    demo: Option<DemoScene>,
    header: String,
    lang: Lang,
    ssh_key: Option<String>,
    trust: Option<CommentTrust>,
    trust_issues: Option<CommentTrust>,
    merge_yes: bool,
    merge_method: Option<String>,
) -> Result<Action, String> {
    if let Some(scene) = demo {
        if auto {
            return Err("--auto cannot be combined with --demo".into());
        }
        if target.is_some() || review || code {
            return Err("pass either a GitHub URL or --demo, not both".into());
        }
        return Ok(Action::Demo {
            scene,
            header: demo_header(scene),
        });
    }
    let Some(target_raw) = target else {
        return Err(if auto {
            "--auto needs github.com/OWNER/REPO/issues".into()
        } else {
            "missing GitHub URL (github.com/OWNER/REPO[/pr/N|/issue/N|/issues])".into()
        });
    };
    if max_iterations == 0 {
        return Err("--max-iterations must be at least 1".into());
    }
    let interval = parse_interval(interval)?;
    let target = parse_target(target_raw)?;
    let mode = mode_for(&target, review, code, auto)?;
    if (merge_yes || merge_method.is_some())
        && !matches!(mode, RunMode::Code | RunMode::Issue | RunMode::Auto)
    {
        return Err(
            "--merge-yes and --merge-method are only valid with --code, an issue URL, or --auto"
                .into(),
        );
    }
    Ok(Action::Run(RunSpec {
        header,
        target,
        mode,
        interval,
        agent,
        yes,
        merge_yes,
        merge_method,
        max_turns: max_iterations,
        lang,
        ssh_key,
        trust: trust.unwrap_or(CommentTrust::All),
        trust_issues: trust_issues.unwrap_or(CommentTrust::Collaborators),
    }))
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    fn gh_flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }
}

fn mode_for(target: &GhTarget, review: bool, code: bool, auto: bool) -> Result<RunMode, String> {
    match target {
        GhTarget::Repo { .. } => {
            if auto {
                return Err("--auto is only valid with github.com/OWNER/REPO/issues".into());
            }
            if review || code {
                return Err(
                    "a repository URL opens the picker. Pass a /pr/N URL to use --review or --code."
                        .into(),
                );
            }
            Ok(RunMode::Picker)
        }
        GhTarget::Issues { .. } => {
            if review || code {
                return Err(
                    "a /issues URL lists open issues. Pass /issue/N to implement one, or add --auto."
                        .into(),
                );
            }
            if auto {
                Ok(RunMode::Auto)
            } else {
                Ok(RunMode::IssueList)
            }
        }
        GhTarget::Issue { .. } => {
            if auto {
                return Err("--auto is only valid with github.com/OWNER/REPO/issues".into());
            }
            if review {
                return Err(
                    "--review applies to a pull request. An issue is implemented, then ghost can continue as --code."
                        .into(),
                );
            }
            Ok(RunMode::Issue)
        }
        GhTarget::Pull { .. } => {
            if auto {
                return Err("--auto is only valid with github.com/OWNER/REPO/issues".into());
            }
            if review {
                Ok(RunMode::Review)
            } else if code {
                Ok(RunMode::Code)
            } else {
                Ok(RunMode::Ask)
            }
        }
    }
}

fn demo_header(scene: DemoScene) -> String {
    match scene {
        DemoScene::Picker => "ghost github.com/cympfh/xxx".into(),
        DemoScene::Review | DemoScene::Preflight => {
            "ghost github.com/cympfh/xxx/pr/3 --review".into()
        }
        DemoScene::Code => "ghost github.com/cympfh/xxx/pr/3 --code".into(),
        DemoScene::Confirm => "ghost github.com/cympfh/xxx/issue/7".into(),
    }
}

fn lang_from_env(flag: Lang) -> Lang {
    let explicit = std::env::args().skip(1).any(|arg| arg_sets_lang(&arg));
    if explicit {
        return flag;
    }
    match std::env::var("GHOST_LANG").ok().as_deref() {
        Some("en") => Lang::En,
        Some("ja") => Lang::Ja,
        _ => flag,
    }
}

fn arg_sets_lang(arg: &str) -> bool {
    arg == "--lang"
        || arg.starts_with("--lang=")
        || arg == "-l"
        || (arg.starts_with("-l") && !arg.starts_with("--"))
}

fn invocation() -> String {
    let mut parts = vec!["ghost".to_string()];
    for arg in std::env::args().skip(1) {
        if arg.contains(' ') {
            parts.push(format!("\"{arg}\""));
        } else {
            parts.push(arg);
        }
    }
    parts.join(" ")
}

#[allow(dead_code)]
fn _duration_hint(d: Duration) -> Duration {
    d
}

// Silence unused import if ValueEnum is only used by clap derive.
#[allow(dead_code)]
fn _value_enum_marker(scene: DemoScene) -> DemoScene {
    let _ = ValueEnum::to_possible_value(&scene);
    scene
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn go(args: &[&str]) -> Result<Action, String> {
        // Skip argv0-style; resolve is what we test.
        let review = args.contains(&"--review");
        let code = args.contains(&"--code");
        let auto = args.contains(&"--auto");
        let yes = args.contains(&"--yes") || args.contains(&"-y");
        let merge_yes = args.contains(&"--merge-yes");
        let merge_method = args
            .windows(2)
            .find(|pair| pair[0] == "--merge-method")
            .map(|pair| match pair[1] {
                "squash" => "--squash",
                "rebase" => "--rebase",
                _ => "--merge",
            })
            .map(str::to_string);
        let target = args
            .iter()
            .copied()
            .find(|a| !a.starts_with('-'))
            .filter(|a| *a != "ghost");
        resolve(
            target,
            review,
            code,
            auto,
            "5m",
            None,
            yes,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            None,
            merge_yes,
            merge_method,
        )
    }

    #[test]
    fn short_flags_parse_and_command_is_consistent() {
        let mut cmd = Cli::command();
        cmd.clone().debug_assert();
        let mut help = Vec::new();
        cmd.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        for needle in [
            "-r, --review",
            "-c, --code",
            "-i, --interval",
            "-a, --agent",
            "-k, --ssh-key",
            "-l, --lang",
            "-y, --yes",
            "--merge-yes",
            "--merge-method",
            "-n, --max-iterations",
            "-t, --trust-comments",
            "--trust-issues",
            "--auto",
            "-d, --demo",
            "-h, --help",
            "-V, --version",
        ] {
            assert!(help.contains(needle), "help missing {needle}\n{help}");
        }
        assert!(
            !help.contains(", --merge-yes"),
            "--merge-yes must not have a short flag\n{help}"
        );
        assert!(
            !help.contains(", --auto"),
            "--auto must not have a short flag\n{help}"
        );
        assert!(!help.contains(", --merge-method"));
        assert!(
            !help.contains(", --trust-issues"),
            "--trust-issues must not have a short flag\n{help}"
        );

        let cli = Cli::try_parse_from([
            "ghost",
            "github.com/a/b/pr/3",
            "-c",
            "-i",
            "30s",
            "-a",
            "claude",
            "-k",
            "/keys/id_ed25519",
            "-l",
            "en",
            "-y",
            "-n",
            "4",
            "-t",
            "all",
        ])
        .unwrap();
        assert!(cli.code);
        assert!(!cli.review);
        assert_eq!(cli.interval, "30s");
        assert_eq!(cli.agent, Some(AgentKind::Claude));
        assert_eq!(cli.ssh_key.as_deref(), Some("/keys/id_ed25519"));
        assert_eq!(cli.lang, Lang::En);
        assert!(cli.yes);
        assert_eq!(cli.max_iterations, 4);
        assert_eq!(cli.trust_comments, Some(CommentTrust::All));
        let default_trust = Cli::try_parse_from(["ghost", "github.com/a/b/pr/1", "-c"]).unwrap();
        assert!(default_trust.trust_comments.is_none());

        let review = Cli::try_parse_from(["ghost", "github.com/a/b/pr/1", "-r"]).unwrap();
        assert!(review.review);
        assert!(!review.code);
        let demo = Cli::try_parse_from(["ghost", "-d", "picker"]).unwrap();
        assert_eq!(demo.demo, Some(DemoScene::Picker));
        assert!(Cli::try_parse_from(["ghost", "github.com/a/b/pr/3", "-r", "-c"]).is_err());
    }

    #[test]
    fn default_interval_is_20_seconds() {
        let cli = Cli::try_parse_from(["ghost", "github.com/a/b/pr/1", "--review"]).unwrap();
        assert_eq!(cli.interval, "20s");
        assert_eq!(cli.lang, Lang::Ja);
        let action = resolve(
            Some("github.com/a/b/pr/1"),
            true,
            false,
            false,
            &cli.interval,
            None,
            false,
            20,
            None,
            "ghost".into(),
            cli.lang,
            None,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        match action {
            Action::Run(spec) => {
                assert_eq!(spec.interval, Duration::from_secs(20));
                assert_eq!(spec.lang, Lang::Ja);
            }
            Action::Demo { .. } => panic!("expected a run"),
        }
    }

    #[test]
    fn routes_each_invocation() {
        assert!(matches!(
            go(&["github.com/a/b"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Picker,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/pr/3", "--review"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Review,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/pr/3", "--code"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Code,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/pr/3"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Ask,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/issue/7", "--yes"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Issue,
                yes: true,
                ..
            })
        ));
        assert!(go(&["github.com/a/b/issue/7", "--review"]).is_err());
        assert!(go(&["github.com/a/b", "--code"]).is_err());
        assert!(go(&[]).is_err());
    }

    #[test]
    fn rejects_zero_iterations_and_bad_interval() {
        assert!(resolve(
            Some("github.com/a/b/pr/1"),
            true,
            false,
            false,
            "5m",
            None,
            false,
            0,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            None,
            false,
            None,
        )
        .is_err());
        assert!(resolve(
            Some("github.com/a/b/pr/1"),
            true,
            false,
            false,
            "nope",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::En,
            None,
            None,
            None,
            false,
            None,
        )
        .is_err());
    }

    #[test]
    fn merge_yes_is_only_for_code_and_issue() {
        assert!(go(&["github.com/a/b/pr/3", "--review", "--merge-yes"]).is_err());
        assert!(go(&["github.com/a/b/pr/3", "--merge-yes"]).is_err());
        assert!(go(&["github.com/a/b", "--merge-yes"]).is_err());
        assert!(matches!(
            go(&["github.com/a/b/pr/3", "--code", "--merge-yes"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Code,
                merge_yes: true,
                yes: false,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/issue/7", "--merge-yes", "--merge-method", "squash"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Issue,
                merge_yes: true,
                merge_method: Some(method),
                ..
            }) if method == "--squash"
        ));
        let cli = Cli::try_parse_from([
            "ghost",
            "github.com/a/b/pr/3",
            "-c",
            "--merge-yes",
            "--merge-method",
            "rebase",
        ])
        .unwrap();
        assert!(cli.merge_yes);
        assert!(matches!(cli.merge_method, Some(MergeMethod::Rebase)));
    }

    #[test]
    fn auto_is_only_for_the_issues_list() {
        assert!(matches!(
            go(&["github.com/a/b/issues"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::IssueList,
                ..
            })
        ));
        assert!(matches!(
            go(&["github.com/a/b/issue"]).unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::IssueList,
                ..
            })
        ));
        assert!(matches!(
            go(&[
                "github.com/a/b/issues",
                "--auto",
                "--yes",
                "--merge-yes",
                "--merge-method",
                "squash"
            ])
            .unwrap(),
            Action::Run(RunSpec {
                mode: RunMode::Auto,
                yes: true,
                merge_yes: true,
                merge_method: Some(method),
                ..
            }) if method == "--squash"
        ));
        assert!(go(&["github.com/a/b", "--auto"]).is_err());
        assert!(go(&["github.com/a/b/pr/3", "--auto"]).is_err());
        assert!(go(&["github.com/a/b/issue/7", "--auto"]).is_err());
        assert!(go(&["github.com/a/b/issues", "--review"]).is_err());
        assert!(go(&["github.com/a/b/issues", "--code"]).is_err());
        assert!(go(&["github.com/a/b/issues", "--merge-yes"]).is_err());
        assert!(go(&["github.com/a/b/pr/3", "--review", "--merge-yes"]).is_err());
        assert!(go(&["--auto"]).is_err());
        let cli = Cli::try_parse_from(["ghost", "github.com/a/b/issues", "--auto"]).unwrap();
        assert!(cli.auto);
        match resolve(
            Some("github.com/a/b/issues"),
            false,
            false,
            true,
            "20s",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            None,
            false,
            None,
        )
        .unwrap()
        {
            Action::Run(spec) => {
                assert_eq!(spec.trust, CommentTrust::All);
                assert_eq!(spec.trust_issues, CommentTrust::Collaborators);
            }
            Action::Demo { .. } => panic!("expected a run"),
        }
        match resolve(
            Some("github.com/a/b/pr/3"),
            false,
            true,
            false,
            "20s",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            None,
            false,
            None,
        )
        .unwrap()
        {
            Action::Run(spec) => assert_eq!(spec.trust, CommentTrust::All),
            Action::Demo { .. } => panic!("expected a run"),
        }
        match resolve(
            Some("github.com/a/b/issues"),
            false,
            false,
            true,
            "20s",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            Some(CommentTrust::All),
            None,
            false,
            None,
        )
        .unwrap()
        {
            Action::Run(spec) => {
                assert_eq!(spec.trust, CommentTrust::All);
                assert_eq!(spec.trust_issues, CommentTrust::Collaborators);
            }
            Action::Demo { .. } => panic!("expected a run"),
        }
        match resolve(
            Some("github.com/a/b/issue/4"),
            false,
            false,
            false,
            "20s",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            None,
            false,
            None,
        )
        .unwrap()
        {
            Action::Run(spec) => assert_eq!(spec.trust, CommentTrust::All),
            Action::Demo { .. } => panic!("expected a run"),
        }
        match resolve(
            Some("github.com/a/b/issues"),
            false,
            false,
            true,
            "20s",
            None,
            false,
            20,
            None,
            "ghost".into(),
            Lang::Ja,
            None,
            None,
            Some(CommentTrust::All),
            false,
            None,
        )
        .unwrap()
        {
            Action::Run(spec) => {
                assert_eq!(spec.trust, CommentTrust::All);
                assert_eq!(spec.trust_issues, CommentTrust::All);
            }
            Action::Demo { .. } => panic!("expected a run"),
        }
    }
}
