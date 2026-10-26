//! Scripted screens for screenshots and for previewing the TUI without GitHub.

use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use clap::ValueEnum;

use crate::github::{ItemKind, OpenItem};
use crate::lang::Lang;
use crate::tui::{
    self, merge_question, LogKind, LogLine, MergeStyle, Side, Status, UiCmd, UiEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DemoScene {
    Picker,
    Review,
    Code,
    Confirm,
    Preflight,
}

pub fn run(scene: DemoScene, header: String) -> anyhow::Result<i32> {
    tui::run(header, move |tx, rx| play(scene, tx, rx))
}

fn play(scene: DemoScene, tx: Sender<UiEvent>, rx: Receiver<UiCmd>) {
    match scene {
        DemoScene::Picker => {
            send_status(
                &tx,
                "picker",
                "github.com/cympfh/xxx",
                "choose",
                "—",
                "—",
                "0/20",
                "auto",
                "—",
            );
            let _ = tx.send(UiEvent::Picker(sample_items()));
            wait_picker(&tx, &rx);
        }
        DemoScene::Review => {
            review_scene(&tx);
            wait_quit(&rx);
        }
        DemoScene::Code => {
            code_scene(&tx);
            wait_quit(&rx);
        }
        DemoScene::Confirm => {
            merge_confirm_scene(&tx);
            wait_merge_confirm(&tx, &rx);
        }
        DemoScene::Preflight => {
            let _ = tx.send(UiEvent::Fatal(
                "\
`gh` is installed but not authenticated.

You are not logged into any GitHub hosts.

Fix:
  gh auth login"
                    .into(),
            ));
            wait_quit(&rx);
        }
    }
}

pub fn sample_items() -> Vec<OpenItem> {
    vec![
        OpenItem {
            kind: ItemKind::Pull,
            number: 3,
            title: "Babysit PRs until they merge".into(),
            author: "cympfh".into(),
            labels: vec!["tui".into(), "rust".into()],
            ci: Some("CI …".into()),
            review: Some("review".into()),
        },
        OpenItem {
            kind: ItemKind::Pull,
            number: 2,
            title: "Parse GitHub URLs".into(),
            author: "ada".into(),
            labels: vec!["bug".into()],
            ci: Some("CI ✓".into()),
            review: Some("approved".into()),
        },
        OpenItem {
            kind: ItemKind::Pull,
            number: 1,
            title: "Initial CLI skeleton".into(),
            author: "lin".into(),
            labels: vec!["wip".into()],
            ci: Some("CI ✗".into()),
            review: Some("changes".into()),
        },
        OpenItem {
            kind: ItemKind::Issue,
            number: 7,
            title: "Continue when a PR already exists".into(),
            author: "cympfh".into(),
            labels: vec!["ux".into()],
            ci: None,
            review: None,
        },
        OpenItem {
            kind: ItemKind::Issue,
            number: 5,
            title: "Show the countdown in the status bar".into(),
            author: "mei".into(),
            labels: vec!["ui".into()],
            ci: None,
            review: None,
        },
    ]
}

fn review_scene(tx: &Sender<UiEvent>) {
    send_status(
        tx, "review", "PR #3", "running", "4f1c2ab", "pending", "1/20", "claude", "7f3ac2e1",
    );
    sys(tx, "gh ok · cursor");
    sys(tx, "workdir ~/.ghost/work/cympfh__xxx (reused clone)");
    sys(tx, "PR #3 head 4f1c2ab · CI pending");
    sys(tx, "starting claude session 7f3ac2e1");
    agent(
        tx,
        "こんにちは。該当のrepositoryが手元にあるかみて、PR#3をなんかしますうんぬん。。。",
    );
    agent(tx, "gh pr diff 3 --repo cympfh/xxx");
    agent(
        tx,
        "差分は 312 行。countdown のテストは足りていそう。コメントします。",
    );
    agent(
        tx,
        "gh pr review 3 --repo cympfh/xxx --comment --body \"interval parsing looks good\"",
    );
}

fn code_scene(tx: &Sender<UiEvent>) {
    send_status(
        tx, "code", "PR #3", "running", "9ab31c0", "failed", "2/20", "claude", "7f3ac2e1",
    );
    sys(tx, "same claude session 7f3ac2e1 (--resume)");
    sys(tx, "CI failed: build");
    sys(tx, "new review comment from @ada");
    agent(
        tx,
        "follow-up in the same session. CI is red on build; I'll read it with gh.",
    );
    agent(tx, "gh pr checks 3 --repo cympfh/xxx");
    agent(
        tx,
        "the interval test expected 300 seconds. fixing interval.rs.",
    );
    agent(tx, "git push origin fix/3-interval");
    agent(
        tx,
        "gh pr comment 3 --repo cympfh/xxx --body \"Fixed in the latest push.\"",
    );
}

fn merge_confirm_scene(tx: &Sender<UiEvent>) {
    send_status(
        tx, "code", "PR #3", "confirm", "abc1234", "none", "1/20", "codex", "—",
    );
    sys(
        tx,
        "merge-ready? CI none(ok) / CLEAN / changes requested 0 → yes",
    );
    let method = MergeStyle::Squash;
    let _ = tx.send(UiEvent::Confirm {
        number: 3,
        message: merge_question(3, method, Lang::Ja),
        kind: tui::ConfirmKind::Merge(method),
        lang: Lang::Ja,
    });
}

fn wait_picker(tx: &Sender<UiEvent>, rx: &Receiver<UiCmd>) {
    loop {
        match rx.recv() {
            Ok(UiCmd::Quit) | Err(_) => {
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: "quit".into(),
                });
                return;
            }
            Ok(UiCmd::Pick { kind, number, side }) => {
                let _ = tx.send(UiEvent::Log(LogLine {
                    kind: LogKind::System,
                    text: format!("selected {kind:?} #{number} {side:?}"),
                }));
                match (kind, side) {
                    (ItemKind::Pull, Some(Side::Code)) => code_scene(tx),
                    (ItemKind::Issue, _) => code_scene(tx),
                    _ => review_scene(tx),
                }
            }
            Ok(UiCmd::Choose(Side::Code)) => code_scene(tx),
            Ok(UiCmd::Choose(Side::Review)) => review_scene(tx),
            Ok(UiCmd::Confirm {
                answer: tui::ConfirmAnswer::Yes,
                ..
            }) => {
                code_scene(tx);
            }
            Ok(UiCmd::Confirm {
                answer: tui::ConfirmAnswer::No | tui::ConfirmAnswer::Continue,
                ..
            }) => {
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: "not continuing as --code".into(),
                });
                return;
            }
            Ok(_) => {}
        }
    }
}

fn wait_merge_confirm(tx: &Sender<UiEvent>, rx: &Receiver<UiCmd>) {
    loop {
        match rx.recv() {
            Ok(UiCmd::Confirm {
                answer: tui::ConfirmAnswer::Yes,
                method,
            }) => {
                let word = method.map(MergeStyle::word).unwrap_or("squash");
                sys(tx, &format!("merge method: --{word}"));
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: format!("merged with {word}"),
                });
                return;
            }
            Ok(UiCmd::Confirm {
                answer: tui::ConfirmAnswer::No,
                ..
            }) => {
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: "PR #3 はマージせずに終了します。".into(),
                });
                return;
            }
            Ok(UiCmd::Confirm {
                answer: tui::ConfirmAnswer::Continue,
                ..
            }) => {
                sys(tx, "監視を続けます。次にマージ可能になったら再度尋ねます。");
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: "continue".into(),
                });
                return;
            }
            Ok(UiCmd::Quit) | Err(_) => {
                let _ = tx.send(UiEvent::Exit {
                    code: 0,
                    message: "quit".into(),
                });
                return;
            }
            Ok(_) => {}
        }
    }
}

fn wait_quit(rx: &Receiver<UiCmd>) {
    // Keep the screen up. A demo capture only needs the first frames.
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiCmd::Quit) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(UiCmd::Interrupt) => {}
            Ok(UiCmd::Input(text)) => {
                let _ = text;
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_status(
    tx: &Sender<UiEvent>,
    mode: &str,
    target: &str,
    phase: &str,
    sha: &str,
    ci: &str,
    turns: &str,
    agent: &str,
    session: &str,
) {
    let _ = tx.send(UiEvent::Status(Status {
        mode: mode.into(),
        target: target.into(),
        phase: phase.into(),
        sha: sha.into(),
        ci: ci.into(),
        turns: turns.into(),
        agent: agent.into(),
        session: session.into(),
    }));
}

fn sys(tx: &Sender<UiEvent>, text: &str) {
    let _ = tx.send(UiEvent::Log(LogLine {
        kind: LogKind::System,
        text: text.into(),
    }));
}

fn agent(tx: &Sender<UiEvent>, text: &str) {
    let _ = tx.send(UiEvent::Log(LogLine {
        kind: LogKind::Agent,
        text: text.into(),
    }));
}

/// Used by the screenshot harness so a scene can be held open from another thread.
#[allow(dead_code)]
pub fn hold() {
    thread::sleep(Duration::from_secs(3600));
}
