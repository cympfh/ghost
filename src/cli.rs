use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};

use crate::agent::AgentKind;
use crate::demo::{self, DemoScene};
use crate::engine::{self, RunMode, RunSpec};
use crate::interval::parse_interval;
use crate::target::{parse_target, GhTarget};

#[derive(Parser, Debug)]
#[command(
    name = "ghost",
    version,
    about = "Babysit a GitHub PR or issue with one local coding agent until it is merge-ready",
    after_help = "\
Examples:
  ghost github.com/owner/repo
  ghost github.com/owner/repo/pr/3 --review
  ghost github.com/owner/repo/pr/3 --code --interval 2m
  ghost github.com/owner/repo/issue/7 --yes
  ghost --demo review

GitHub access is only through the `gh` CLI. One agent session is started for the
run; later updates resume it (claude --resume, codex exec resume, grok --resume)."
)]
struct Cli {
    /// github.com/OWNER/REPO, .../pr/N, or .../issue/N
    target: Option<String>,

    /// Review the pull request when its head SHA changes.
    #[arg(long, conflicts_with = "code")]
    review: bool,

    /// Update the pull request when CI fails or new review comments appear.
    #[arg(long, conflicts_with = "review")]
    code: bool,

    /// How often to poll `gh`. First check is immediate. Bare numbers are seconds.
    #[arg(long, default_value = "5m")]
    interval: String,

    /// Coding agent. Default: first of codex, claude, grok on PATH.
    #[arg(long, value_enum)]
    agent: Option<AgentKind>,

    /// Accept the issue-mode prompt to continue as --code.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Stop after this many agent turns so --review and --code cannot ping-pong forever.
    #[arg(long, default_value_t = 20)]
    max_iterations: u32,

    /// Draw a scripted screen (picker, review, code, confirm, preflight).
    #[arg(long, value_enum)]
    demo: Option<DemoScene>,
}

pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let header = invocation();
    match resolve(
        cli.target.as_deref(),
        cli.review,
        cli.code,
        &cli.interval,
        cli.agent,
        cli.yes,
        cli.max_iterations,
        cli.demo,
        header,
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
    interval: &str,
    agent: Option<AgentKind>,
    yes: bool,
    max_iterations: u32,
    demo: Option<DemoScene>,
    header: String,
) -> Result<Action, String> {
    if let Some(scene) = demo {
        if target.is_some() || review || code {
            return Err("pass either a GitHub URL or --demo, not both".into());
        }
        return Ok(Action::Demo {
            scene,
            header: demo_header(scene),
        });
    }
    let Some(target_raw) = target else {
        return Err("missing GitHub URL (github.com/OWNER/REPO[/pr/N|/issue/N])".into());
    };
    if max_iterations == 0 {
        return Err("--max-iterations must be at least 1".into());
    }
    let interval = parse_interval(interval)?;
    let target = parse_target(target_raw)?;
    let mode = mode_for(&target, review, code)?;
    Ok(Action::Run(RunSpec {
        header,
        target,
        mode,
        interval,
        agent,
        yes,
        max_turns: max_iterations,
    }))
}

fn mode_for(target: &GhTarget, review: bool, code: bool) -> Result<RunMode, String> {
    match target {
        GhTarget::Repo { .. } => {
            if review || code {
                return Err(
                    "a repository URL opens the picker. Pass a /pr/N URL to use --review or --code."
                        .into(),
                );
            }
            Ok(RunMode::Picker)
        }
        GhTarget::Issue { .. } => {
            if review {
                return Err(
                    "--review applies to a pull request. An issue is implemented, then ghost can continue as --code."
                        .into(),
                );
            }
            Ok(RunMode::Issue)
        }
        GhTarget::Pull { .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn go(args: &[&str]) -> Result<Action, String> {
        // Skip argv0-style; resolve is what we test.
        let review = args.contains(&"--review");
        let code = args.contains(&"--code");
        let yes = args.contains(&"--yes") || args.contains(&"-y");
        let target = args
            .iter()
            .copied()
            .find(|a| !a.starts_with('-'))
            .filter(|a| *a != "ghost");
        resolve(
            target,
            review,
            code,
            "5m",
            None,
            yes,
            20,
            None,
            "ghost".into(),
        )
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
            "5m",
            None,
            false,
            0,
            None,
            "ghost".into()
        )
        .is_err());
        assert!(resolve(
            Some("github.com/a/b/pr/1"),
            true,
            false,
            "nope",
            None,
            false,
            20,
            None,
            "ghost".into()
        )
        .is_err());
    }
}

// Silence unused import if ValueEnum is only used by clap derive.
#[allow(dead_code)]
fn _value_enum_marker(scene: DemoScene) -> DemoScene {
    let _ = ValueEnum::to_possible_value(&scene);
    scene
}
