//! Poll GitHub with `gh`. Talk to the agent only when something changed,
//! always in the one session started for this run.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::agent::{
    agent_on_path, counts_as_agent_turn, describe_launch, pick_agent, record_progress, AgentKind,
    AgentSession, Launch,
};
use crate::github::Gh;
use crate::lang::Lang;
use crate::preflight::{evaluate, failure_message, is_fatal, AuthProbe, RepoProbe};
use crate::prompt::{self, truncate};
use crate::setup::{SetupFailure, SetupStep};
use crate::shell::{self, which, PromptMode};
use crate::state::{self, GhostState, SessionSlot};
use crate::target::GhTarget;
use crate::text::{format_countdown, short_sha};
use crate::tui::{self, LogKind, LogLine, Side, Status, UiCmd, UiEvent};
use crate::watch::{
    baseline_agent_comments, decide_code, decide_review, ensure_closing_line, feedback_log,
    filter_notes, mark_viewer, CiHandled, CodeAction, CommentTrust, MaxTurns, Note, PrSnap,
    ReviewAction,
};
use crate::workdir::{self, Checkout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Review,
    Code,
    Issue,
    Picker,
    Ask,
}

pub struct RunSpec {
    pub header: String,
    pub target: GhTarget,
    pub mode: RunMode,
    pub interval: Duration,
    pub agent: Option<AgentKind>,
    pub yes: bool,
    pub max_turns: u32,
    pub lang: Lang,
    pub ssh_key: Option<String>,
    pub trust: CommentTrust,
}

pub fn run(spec: RunSpec) -> Result<i32> {
    shell::set_ssh_key(spec.ssh_key.clone());
    crate::agent::install_fatal_signals();
    if spec.mode == RunMode::Picker {
        if let Some(message) = skip_empty_picker(&spec)? {
            println!("{message}");
            return Ok(0);
        }
    }
    let header = spec.header.clone();
    let mut failures = Vec::new();
    let login = match classify_startup(&spec.target.slug(), spec.lang) {
        Startup::Fatal(message) => return show_fatal(header, message),
        Startup::Continue { login, failure } => {
            if let Some(failure) = failure {
                failures.push(failure);
            }
            login
        }
    };
    // Clone, fetch, and checkout on the normal terminal so ssh can ask for a
    // key passphrase before raw mode / the alternate screen take the tty.
    let home = workdir::ghost_home()?;
    let path = workdir::work_path(&home, spec.target.owner(), spec.target.repo());
    eprintln!(
        "ghost: preparing {} (ssh may ask for a key passphrase)",
        path.display()
    );
    let prepared = workdir::prepare(&home, spec.target.owner(), spec.target.repo());
    if !prepared.note.is_empty() {
        eprintln!("ghost: {}", prepared.note);
    }
    for failure in &prepared.failures {
        eprintln!("ghost: {}", prompt::setup_warning(failure));
    }
    failures.extend(prepared.failures);
    let main_clone = prepared.path;
    let slot = activate_slot(&spec, &home, &main_clone);
    for failure in &slot.failures {
        eprintln!("ghost: {}", prompt::setup_warning(failure));
    }
    failures.extend(slot.failures);
    let mut workdir = slot.workdir;
    let slot_dir = slot.intended;
    if spec.mode != RunMode::Picker && slot.lock.is_some() {
        let primed = prime_checkout(&spec, &main_clone, &slot_dir);
        if !primed.note.is_empty() {
            eprintln!("ghost: {}", primed.note);
        }
        for failure in &primed.failures {
            eprintln!("ghost: {}", prompt::setup_warning(failure));
        }
        failures.extend(primed.failures);
        workdir = primed.workdir;
    }
    let how = prepared.note;
    let workdir_for_ui = workdir.clone();
    let main_for_ui = main_clone.clone();
    let home_for_ui = home.clone();
    let slot_for_ui = slot_dir.clone();
    let failures_for_ui = failures;
    let slot_lock = slot.lock;
    tui::run(header, move |tx, rx| {
        if let Err(err) = babysit(
            spec,
            login,
            home_for_ui,
            main_for_ui,
            workdir_for_ui,
            slot_for_ui,
            slot_lock,
            how,
            failures_for_ui,
            tx.clone(),
            rx,
        ) {
            let message = format!("{err:#}");
            let _ = tx.send(UiEvent::Fatal(message.clone()));
            let _ = tx.send(UiEvent::Exit { code: 1, message });
        }
    })
}

fn show_fatal(header: String, message: String) -> Result<i32> {
    tui::run(header, move |tx, rx| {
        let _ = tx.send(UiEvent::Fatal(message.clone()));
        wait_quit(&rx);
        let _ = tx.send(UiEvent::Exit { code: 1, message });
    })
}

fn prime_checkout(
    spec: &RunSpec,
    main: &std::path::Path,
    dest: &std::path::Path,
) -> workdir::CheckoutOutcome {
    let slug = spec.target.slug();
    let gh = Gh::new(slug.clone());
    match spec.mode {
        RunMode::Review | RunMode::Code | RunMode::Ask => {
            let number = match pull_number(&spec.target) {
                Ok(number) => number,
                Err(err) => {
                    return failed_checkout(
                        main,
                        dest,
                        plain_failure(
                            "対象がプルリクエストか確認するため",
                            "parse pull request url",
                            "PR 番号が分かっている状態にしてから作業して",
                            &err,
                        ),
                    )
                }
            };
            let purpose = pr_purpose(spec.mode, number);
            match gh.head_branch(number, &purpose) {
                Ok(branch) => workdir::sync_and_checkout(
                    main,
                    dest,
                    &slug,
                    Checkout::Pr {
                        number,
                        branch: Some(branch),
                    },
                    PromptMode::Tty,
                    &purpose,
                ),
                Err(failure) => {
                    let mut outcome = workdir::sync_and_checkout(
                        main,
                        dest,
                        &slug,
                        Checkout::Pr {
                            number,
                            branch: None,
                        },
                        PromptMode::Tty,
                        &purpose,
                    );
                    outcome.failures.insert(0, failure);
                    outcome
                }
            }
        }
        RunMode::Issue => prime_issue(&gh, &slug, spec, main, dest),
        RunMode::Picker => workdir::CheckoutOutcome {
            workdir: dest.to_path_buf(),
            note: String::new(),
            failures: Vec::new(),
        },
    }
}

fn prime_issue(
    gh: &Gh,
    slug: &str,
    spec: &RunSpec,
    main: &std::path::Path,
    dest: &std::path::Path,
) -> workdir::CheckoutOutcome {
    let number = match issue_number(&spec.target) {
        Ok(number) => number,
        Err(err) => {
            return failed_checkout(
                main,
                dest,
                plain_failure(
                    "対象が issue か確認するため",
                    "parse issue url",
                    "issue 番号が分かっている状態にしてから作業して",
                    &err,
                ),
            )
        }
    };
    let purpose = format!("issue #{number}");
    let issue = match gh.issue(number) {
        Ok(issue) => issue,
        Err(err) => {
            return failed_checkout(
                main,
                dest,
                gh_failure(
                    format!("{purpose} の内容を読むため"),
                    format!("gh issue view {number} --repo {slug} --json number,title,body,state,labels"),
                    format!("{purpose} のタイトルと本文が分かっている状態にしてから作業して"),
                    &err,
                ),
            );
        }
    };
    let branch = workdir::issue_branch(number, &issue.title, &label_refs(&issue.labels));
    match gh.find_issue_pr(number, &branch) {
        Ok(Some(pr)) => {
            let purpose = pr_purpose(RunMode::Issue, pr);
            match gh.head_branch(pr, &purpose) {
                Ok(head) => workdir::sync_and_checkout(
                    main,
                    dest,
                    slug,
                    Checkout::Pr {
                        number: pr,
                        branch: Some(head),
                    },
                    PromptMode::Tty,
                    &purpose,
                ),
                Err(failure) => {
                    let mut outcome = workdir::sync_and_checkout(
                        main,
                        dest,
                        slug,
                        Checkout::Pr {
                            number: pr,
                            branch: None,
                        },
                        PromptMode::Tty,
                        &purpose,
                    );
                    outcome.failures.insert(0, failure);
                    outcome
                }
            }
        }
        Ok(None) => {
            let mut extra = Vec::new();
            let base = match gh.default_branch() {
                Ok(base) => base,
                Err(err) => {
                    extra.push(gh_failure(
                        format!("{purpose} のデフォルトブランチを知るため"),
                        format!("gh repo view {slug} --json defaultBranchRef"),
                        format!("{purpose} の base ブランチが分かっている状態にしてから作業して"),
                        &err,
                    ));
                    "main".into()
                }
            };
            let mut outcome = workdir::sync_and_checkout(
                main,
                dest,
                slug,
                Checkout::Issue { branch, base },
                PromptMode::Tty,
                &purpose,
            );
            for failure in extra.into_iter().rev() {
                outcome.failures.insert(0, failure);
            }
            outcome
        }
        Err(err) => failed_checkout(
            main,
            dest,
            gh_failure(
                format!("{purpose} の既存プルリクエストを探すため"),
                format!("gh pr list --repo {slug} --head {branch}"),
                format!("{purpose} の既存 PR があるかどうか分かっている状態にしてから作業して"),
                &err,
            ),
        ),
    }
}

struct SlotPrep {
    intended: PathBuf,
    workdir: PathBuf,
    lock: Option<workdir::SlotLock>,
    failures: Vec<SetupFailure>,
}

fn activate_slot(spec: &RunSpec, home: &std::path::Path, main: &std::path::Path) -> SlotPrep {
    if spec.mode == RunMode::Picker {
        return SlotPrep {
            intended: main.to_path_buf(),
            workdir: main.to_path_buf(),
            lock: None,
            failures: Vec::new(),
        };
    }
    let slot = match spec.mode {
        RunMode::Issue => match issue_number(&spec.target) {
            Ok(number) => format!("issue-{number}"),
            Err(err) => {
                return SlotPrep {
                    intended: main.to_path_buf(),
                    workdir: main.to_path_buf(),
                    lock: None,
                    failures: vec![plain_failure(
                        "対象の番号を読むため",
                        "parse issue url",
                        "issue 番号が分かっている状態にしてから作業して",
                        &err,
                    )],
                };
            }
        },
        _ => match pull_number(&spec.target) {
            Ok(number) => format!("pr-{number}"),
            Err(err) => {
                return SlotPrep {
                    intended: main.to_path_buf(),
                    workdir: main.to_path_buf(),
                    lock: None,
                    failures: vec![plain_failure(
                        "対象の番号を読むため",
                        "parse pull request url",
                        "PR 番号が分かっている状態にしてから作業して",
                        &err,
                    )],
                };
            }
        },
    };
    let dest = workdir::slot_path(home, spec.target.owner(), spec.target.repo(), &slot);
    match workdir::lock_slot(&dest) {
        Ok(lock) => SlotPrep {
            workdir: workdir::best_workdir(&dest, main, home),
            intended: dest,
            lock: Some(lock),
            failures: Vec::new(),
        },
        Err(err) => SlotPrep {
            intended: main.to_path_buf(),
            workdir: main.to_path_buf(),
            lock: None,
            failures: vec![plain_failure(
                format!("{slot} の作業ディレクトリを排他で使うため"),
                format!("flock {}", dest.display()),
                format!(
                    "{} をこのプロセスだけで使える状態にしてから作業して",
                    dest.display()
                ),
                &err,
            )],
        },
    }
}

fn babysit(
    spec: RunSpec,
    login: Option<String>,
    home: PathBuf,
    main_clone: PathBuf,
    workdir: PathBuf,
    slot_dir: PathBuf,
    slot_lock: Option<workdir::SlotLock>,
    how: String,
    setup_failures: Vec<SetupFailure>,
    tx: Sender<UiEvent>,
    rx: Receiver<UiCmd>,
) -> Result<()> {
    let mut status = Status {
        mode: mode_name(spec.mode).into(),
        target: spec.target.display_url(),
        phase: "starting".into(),
        ..Status::default()
    };
    let _ = tx.send(UiEvent::Status(status.clone()));
    log(
        &tx,
        LogKind::System,
        match &login {
            Some(name) => format!("gh ok · {name}"),
            None => "gh ok".into(),
        },
    );
    log(
        &tx,
        LogKind::System,
        format!("workdir {} ({how})", workdir.display()),
    );
    for failure in &setup_failures {
        log(&tx, LogKind::Error, prompt::setup_warning(failure));
    }

    let kind = pick_agent(spec.agent, agent_on_path).map_err(|e| anyhow!(e))?;
    status.agent = kind.to_string();
    let _ = tx.send(UiEvent::Status(status.clone()));
    log(&tx, LogKind::System, format!("agent {kind}"));

    let slug = spec.target.slug();
    let owner = spec.target.owner().to_string();
    let repo = spec.target.repo().to_string();
    let mut ctx = Ctx {
        gh: Gh::new(slug.clone()),
        slug,
        owner,
        repo,
        agent: AgentSession::new(kind, workdir.clone(), &home),
        workdir,
        slot_dir,
        main_clone,
        slot_lock,
        setup_failures,
        setup_handed: false,
        login,
        spec,
        tx,
        rx,
        turns: 0,
        fail_streak: 0,
        queued: Vec::new(),
        status,
        quit: false,
        interrupted: false,
        last_idle_sha: None,
        state_root: state::state_dir(),
    };

    match ctx.spec.mode {
        RunMode::Review => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.review_loop(n)?;
        }
        RunMode::Code => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.code_loop(n)?;
        }
        RunMode::Issue => {
            let n = issue_number(&ctx.spec.target)?;
            ctx.issue_flow(n)?;
        }
        RunMode::Ask => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.ask_mode(n)?;
        }
        RunMode::Picker => ctx.picker_flow()?,
    }
    Ok(())
}

struct Ctx {
    spec: RunSpec,
    tx: Sender<UiEvent>,
    rx: Receiver<UiCmd>,
    gh: Gh,
    slug: String,
    owner: String,
    repo: String,
    workdir: PathBuf,
    slot_dir: PathBuf,
    main_clone: PathBuf,
    setup_failures: Vec<SetupFailure>,
    setup_handed: bool,
    #[allow(dead_code)]
    slot_lock: Option<workdir::SlotLock>,
    login: Option<String>,
    agent: AgentSession,
    turns: u32,
    /// Real agent failures in a row. A successful turn clears it.
    fail_streak: u32,
    queued: Vec<String>,
    status: Status,
    quit: bool,
    interrupted: bool,
    last_idle_sha: Option<String>,
    state_root: PathBuf,
}

impl Ctx {
    fn review_loop(&mut self, number: u64) -> Result<()> {
        self.status.mode = "review".into();
        self.status.target = format!("PR #{number}");
        let path = state::pr_state_path(&self.state_root, &self.owner, &self.repo, number);
        let mut stored = GhostState::load(&path)?;
        self.agent
            .seed(stored.session_for(SessionSlot::Review, self.agent.kind()));
        self.push_status();
        loop {
            if self.quit {
                return Ok(());
            }
            let snap = match self.poll_pr(number)? {
                Some(snap) => snap,
                None => return Ok(()),
            };
            match decide_review(
                &snap,
                stored.last_reviewed_sha.as_deref(),
                self.turns,
                self.spec.max_turns,
            ) {
                Err(MaxTurns) => {
                    self.stop(3, self.max_message());
                    return Ok(());
                }
                Ok(ReviewAction::Stop(reason)) => {
                    self.stop(reason.exit_code(), reason.message(self.spec.lang, number));
                    return Ok(());
                }
                Ok(ReviewAction::Idle) => {
                    if !self.delegate_setup()? {
                        self.note_idle(&snap.head_sha);
                    }
                }
                Ok(ReviewAction::Review { .. }) => {
                    self.checkout(
                        Checkout::Pr {
                            number,
                            branch: Some(snap.head_branch.clone()),
                        },
                        &pr_purpose(RunMode::Review, number),
                    );
                    if self.quit {
                        return Ok(());
                    }
                    let diff = self.load_diff(
                        number,
                        stored.last_reviewed_sha.as_deref(),
                        &snap.head_sha,
                    )?;
                    let notes = std::mem::take(&mut self.queued);
                    let task = self.decorate(match self.agent.session_id() {
                        Some(_) => prompt::review_followup(
                            &self.slug,
                            number,
                            stored.last_reviewed_sha.as_deref().unwrap_or("unreviewed"),
                            &snap.head_sha,
                            &self.workdir,
                            &diff,
                            &notes,
                            self.spec.lang,
                        ),
                        None => prompt::review_initial(
                            &self.slug,
                            number,
                            &snap.head_sha,
                            &self.workdir,
                            &diff,
                            &notes,
                            self.spec.lang,
                        ),
                    });
                    let ok = self.dispatch_task(&task)?;
                    self.note_setup_result(ok);
                    if self.quit {
                        return Ok(());
                    }
                    if ok {
                        stored.last_reviewed_sha = Some(snap.head_sha.clone());
                        self.remember(&mut stored, SessionSlot::Review);
                        stored.save(&path)?;
                        self.log(format!(
                            "recorded reviewed SHA {}",
                            short_sha(&snap.head_sha)
                        ));
                    } else {
                        self.log(self.say(
                            "エージェントのターンが失敗したので、レビュー済み SHA は記録しません",
                            "agent turn failed; not recording the reviewed SHA",
                        ));
                    }
                }
            }
            if self.wait_interval()? {
                return Ok(());
            }
        }
    }

    fn code_loop(&mut self, number: u64) -> Result<()> {
        self.status.mode = "code".into();
        self.status.target = format!("PR #{number}");
        let path = state::pr_state_path(&self.state_root, &self.owner, &self.repo, number);
        let mut stored = GhostState::load(&path)?;
        // Do not replace a session that issue mode already started.
        self.agent
            .seed(stored.session_for(SessionSlot::Code, self.agent.kind()));
        let mut seen: BTreeSet<String> = stored.seen_comment_ids.iter().cloned().collect();
        let mut agent_ids: BTreeSet<String> = stored.agent_comment_ids.iter().cloned().collect();
        self.push_status();
        loop {
            if self.quit {
                return Ok(());
            }
            let snap = match self.poll_pr(number)? {
                Some(mut snap) => {
                    if let Some(line) = self.prepare_notes(&mut snap, &agent_ids) {
                        self.log(line);
                    }
                    snap
                }
                None => return Ok(()),
            };
            let handled = ci_handled(&stored);
            match decide_code(
                &snap,
                &seen,
                self.turns,
                self.spec.max_turns,
                handled.as_ref(),
            ) {
                Err(MaxTurns) => {
                    self.stop(3, self.max_message());
                    return Ok(());
                }
                Ok(CodeAction::Stop(reason)) => {
                    self.stop(reason.exit_code(), reason.message(self.spec.lang, number));
                    return Ok(());
                }
                Ok(CodeAction::Idle) => {
                    if !self.delegate_setup()? {
                        self.note_code_idle(&snap);
                    }
                }
                Ok(CodeAction::Fix {
                    ci_failed,
                    new_notes,
                }) => {
                    self.announce_fix(ci_failed, &snap, &new_notes);
                    self.checkout(
                        Checkout::Pr {
                            number,
                            branch: Some(snap.head_branch.clone()),
                        },
                        &pr_purpose(RunMode::Code, number),
                    );
                    if self.quit {
                        return Ok(());
                    }
                    let user_notes = std::mem::take(&mut self.queued);
                    let task = self.decorate(if self.agent.session_id().is_some() {
                        prompt::code_followup(
                            &self.slug,
                            number,
                            &snap.head_branch,
                            &snap.head_sha,
                            &self.workdir,
                            ci_failed,
                            &snap.failing_checks,
                            &new_notes,
                            &user_notes,
                            self.spec.lang,
                        )
                    } else {
                        prompt::code_initial(
                            &self.slug,
                            number,
                            &snap.head_branch,
                            &snap.head_sha,
                            &self.workdir,
                            ci_failed,
                            &snap.failing_checks,
                            &new_notes,
                            &user_notes,
                            self.spec.lang,
                        )
                    });
                    let prompted: BTreeSet<String> =
                        new_notes.iter().map(|n| n.id.clone()).collect();
                    let ok = self.dispatch_task(&task)?;
                    self.note_setup_result(ok);
                    if self.quit {
                        return Ok(());
                    }
                    if ok {
                        for id in &prompted {
                            seen.insert(id.clone());
                        }
                        if let Ok(fresh) = self.gh.pr(number) {
                            let login = self.login();
                            for note in fresh.notes {
                                let arrived_during_turn = !prompted.contains(&note.id);
                                if arrived_during_turn && crate::watch::author_is(&note, &login) {
                                    agent_ids.insert(note.id.clone());
                                    seen.insert(note.id);
                                }
                            }
                        }
                        if snap.merge.checks == crate::merge::ChecksVerdict::Failed {
                            stored.handled_ci_sha = Some(snap.head_sha.clone());
                            stored.handled_failing_checks = snap.failing_checks.clone();
                        } else {
                            stored.handled_ci_sha = None;
                            stored.handled_failing_checks.clear();
                        }
                        stored.seen_comment_ids = seen.iter().cloned().collect();
                        stored.agent_comment_ids = agent_ids.iter().cloned().collect();
                        self.remember(&mut stored, SessionSlot::Code);
                        stored.save(&path)?;
                        self.log(self.say(
                            "対応したコメントだけを記録しました",
                            "recorded the comments this turn handled",
                        ));
                    } else {
                        self.log(self.say(
                            "エージェントのターンが失敗したので、コメントも CI も処理済みにしません",
                            "agent turn failed; not marking comments or CI as handled",
                        ));
                    }
                }
            }
            if self.wait_interval()? {
                return Ok(());
            }
        }
    }

    fn issue_flow(&mut self, number: u64) -> Result<()> {
        self.status.mode = "issue".into();
        self.status.target = format!("issue #{number}");
        self.push_status();
        let issue = match self.gh.issue(number) {
            Ok(issue) => issue,
            Err(err) => {
                self.push_setup_failure(gh_failure(
                    format!("issue #{number} の内容を読むため"),
                    format!(
                        "gh issue view {number} --repo {} --json number,title,body,state,labels",
                        self.slug
                    ),
                    format!("issue #{number} のタイトルと本文が分かっている状態にしてから作業して"),
                    &err,
                ));
                crate::github::IssueDetails {
                    number,
                    title: String::new(),
                    body: String::new(),
                    state: String::new(),
                    labels: Vec::new(),
                }
            }
        };
        let branch = workdir::issue_branch(number, &issue.title, &label_refs(&issue.labels));
        let existing = match self.gh.find_issue_pr(number, &branch) {
            Ok(pr) => pr,
            Err(err) => {
                self.push_setup_failure(gh_failure(
                    format!("issue #{number} の既存プルリクエストを探すため"),
                    format!("gh pr list --repo {} --head {branch}", self.slug),
                    format!(
                        "issue #{number} の既存 PR があるかどうか分かっている状態にしてから作業して"
                    ),
                    &err,
                ));
                None
            }
        };
        if let Some(pr) = existing {
            self.log(format!(
                "found existing PR #{pr} for issue #{number}; not opening a duplicate"
            ));
            self.baseline_new_pr(pr, false)?;
            return self.confirm_and_code(pr);
        }
        let base = match self.gh.default_branch() {
            Ok(base) => base,
            Err(err) => {
                self.push_setup_failure(gh_failure(
                    format!("issue #{number} のデフォルトブランチを知るため"),
                    format!("gh repo view {} --json defaultBranchRef", self.slug),
                    format!("issue #{number} の base ブランチが分かっている状態にしてから作業して"),
                    &err,
                ));
                "main".into()
            }
        };
        let ipath = state::issue_state_path(&self.state_root, &self.owner, &self.repo, number);
        let mut istate = GhostState::load(&ipath)?;
        self.agent
            .seed(istate.session_for(SessionSlot::Code, self.agent.kind()));
        self.checkout(
            Checkout::Issue {
                branch: branch.clone(),
                base: base.clone(),
            },
            &format!("issue #{number}"),
        );
        if self.quit {
            return Ok(());
        }
        let user_notes = std::mem::take(&mut self.queued);
        let mut task = prompt::issue_initial(
            &self.slug,
            number,
            &issue.title,
            &issue.body,
            &branch,
            &base,
            &self.workdir,
            &user_notes,
            self.spec.lang,
        );
        if self.agent.session_id().is_some() {
            task = format!(
                "{}{task}",
                prompt::issue_resume_prefix(self.spec.lang, number, &branch)
            );
        }
        let task = self.decorate(task);
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        if self.quit {
            return Ok(());
        }
        self.remember(&mut istate, SessionSlot::Code);
        istate.save(&ipath)?;
        let Some(pr) = self.find_pr_after(number, &branch)? else {
            return Ok(());
        };
        if self.quit {
            return Ok(());
        }
        self.ensure_pr_closes(pr, number);
        self.baseline_new_pr(pr, true)?;
        self.log(format!(
            "opened PR #{pr} · keeping this {} session",
            self.agent.kind()
        ));
        self.confirm_and_code(pr)
    }

    /// Seen-set stays empty. Only viewer comments that predate the PR are agent ids.
    fn baseline_new_pr(&mut self, pr: u64, created_here: bool) -> Result<()> {
        let ppath = state::pr_state_path(&self.state_root, &self.owner, &self.repo, pr);
        let mut pstate = GhostState::load(&ppath)?;
        if created_here || pstate.seen_comment_ids.is_empty() {
            let snap = self.gh.pr(pr)?;
            self.log(feedback_log(snap.feedback));
            let login = self.login();
            let baseline =
                baseline_agent_comments(&snap.notes, &login, Some(snap.created_at.as_str()));
            pstate.seen_comment_ids.clear();
            pstate.agent_comment_ids = baseline.into_iter().collect();
        }
        self.remember(&mut pstate, SessionSlot::Code);
        pstate.save(&ppath)?;
        Ok(())
    }

    fn ensure_pr_closes(&mut self, pr: u64, issue: u64) {
        let body = match self.gh.pr_body(pr) {
            Ok(body) => body,
            Err(err) => {
                self.log_kind(
                    LogKind::Error,
                    format!("could not read PR #{pr} body: {err:#}"),
                );
                return;
            }
        };
        let next = ensure_closing_line(&body, issue);
        if next == body {
            return;
        }
        match self.gh.set_pr_body(pr, &next) {
            Ok(()) => self.log(format!("appended `Closes #{issue}` to PR #{pr}")),
            Err(err) => self.log_kind(
                LogKind::Error,
                format!("could not append `Closes #{issue}` to PR #{pr}: {err:#}"),
            ),
        }
    }

    fn confirm_and_code(&mut self, pr: u64) -> Result<()> {
        let message = self.say(
            &format!("PR #{pr} を --code として続けますか？ [Y/n]"),
            &format!("Continue as --code on PR #{pr}? [Y/n]"),
        );
        let yes = if self.spec.yes {
            self.log(format!("{message} yes (--yes)"));
            true
        } else {
            let _ = self.tx.send(UiEvent::Confirm {
                number: pr,
                message: message.clone(),
            });
            self.status.phase = "confirm".into();
            self.push_status();
            match self.wait_confirm()? {
                None => return Ok(()),
                Some(answer) => answer,
            }
        };
        if !yes {
            self.stop(0, format!("PR #{pr} left as-is. Not continuing as --code."));
            return Ok(());
        }
        self.log(format!(
            "continuing as --code on PR #{pr} in the same {} session",
            self.agent.kind()
        ));
        // Same AgentSession. code_loop only seeds an id when this one has none.
        self.code_loop(pr)
    }

    fn ask_mode(&mut self, number: u64) -> Result<()> {
        let title = self.gh.pr(number).map(|p| p.title).unwrap_or_default();
        let _ = self.tx.send(UiEvent::ChooseMode { number, title });
        self.status.mode = "pr".into();
        self.status.target = format!("PR #{number}");
        self.status.phase = "choose".into();
        self.push_status();
        loop {
            match self.rx.recv() {
                Ok(UiCmd::Choose(Side::Review)) => return self.review_loop(number),
                Ok(UiCmd::Choose(Side::Code)) => return self.code_loop(number),
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(other) => self.handle_cmd(other)?,
            }
        }
    }

    fn picker_flow(&mut self) -> Result<()> {
        self.status.mode = "picker".into();
        self.status.target = self.spec.target.display_url();
        self.status.phase = "loading".into();
        self.push_status();
        self.log("loading open pull requests and issues");
        let items = self.gh.open_items()?;
        let _ = self.tx.send(UiEvent::Picker(items));
        self.status.phase = "choose".into();
        self.push_status();
        let choice = loop {
            match self.rx.recv() {
                Ok(UiCmd::Pick { kind, number, side }) => break Some((kind, number, side)),
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(other) => self.handle_cmd(other)?,
            }
        };
        let Some((kind, number, side)) = choice else {
            return Ok(());
        };
        let slot = match kind {
            crate::github::ItemKind::Pull => format!("pr-{number}"),
            crate::github::ItemKind::Issue => format!("issue-{number}"),
        };
        self.use_slot(&slot);
        match (kind, side) {
            (crate::github::ItemKind::Pull, Some(Side::Review)) => self.review_loop(number),
            (crate::github::ItemKind::Pull, Some(Side::Code)) => self.code_loop(number),
            (crate::github::ItemKind::Issue, _) => self.issue_flow(number),
            (crate::github::ItemKind::Pull, None) => self.ask_mode(number),
        }
    }

    fn poll_pr(&mut self, number: u64) -> Result<Option<PrSnap>> {
        loop {
            if self.quit {
                return Ok(None);
            }
            self.status.phase = "checking".into();
            self.push_status();
            match self.gh.pr(number) {
                Ok(snap) => {
                    self.status.sha = short_sha(&snap.head_sha).to_string();
                    self.status.ci = snap.ci_label.clone();
                    self.status.target = format!("PR #{number}");
                    self.push_status();
                    self.log(feedback_log(snap.feedback));
                    for warning in &snap.feedback_warnings {
                        self.log_kind(LogKind::Error, warning.clone());
                    }
                    return Ok(Some(snap));
                }
                Err(err) => {
                    let text = format!("{err:#}");
                    if text.to_ascii_lowercase().contains("could not resolve") {
                        self.stop(1, format!("PR #{number} is not on {}: {text}", self.slug));
                        return Ok(None);
                    }
                    self.log_kind(LogKind::Error, format!("gh pr view: {text}"));
                    if self.wait_interval()? {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn note_idle(&mut self, sha: &str) {
        if self.last_idle_sha.as_deref() != Some(sha) {
            let short = short_sha(sha);
            self.log(self.say(
                &format!(
                    "head {short} は前回レビューから変わっていません。エージェントは呼びません"
                ),
                &format!("head {short} unchanged since the last review; not calling the agent"),
            ));
            self.last_idle_sha = Some(sha.to_string());
        }
    }

    fn note_code_idle(&mut self, snap: &PrSnap) {
        let key = format!("{}:{}", snap.head_sha, snap.ci_label);
        if self.last_idle_sha.as_deref() != Some(&key) {
            self.log(self.say(
                &format!(
                    "PR #{} に新しい更新はありません（CI {}）。エージェントは呼びません",
                    snap.number, snap.ci_label
                ),
                &format!(
                    "nothing new on PR #{} (CI {}); not calling the agent",
                    snap.number, snap.ci_label
                ),
            ));
            self.last_idle_sha = Some(key);
        }
    }

    fn announce_fix(&mut self, ci_failed: bool, snap: &PrSnap, notes: &[Note]) {
        if ci_failed {
            let names = if snap.failing_checks.is_empty() {
                "see gh pr checks".to_string()
            } else {
                snap.failing_checks.join(", ")
            };
            self.log(format!("CI failed: {names}"));
        }
        if !notes.is_empty() {
            self.log(format!("{} new review comment(s)", notes.len()));
        }
    }

    fn checkout(&mut self, checkout: Checkout, purpose: &str) {
        if self.slot_dir == self.main_clone {
            self.push_setup_failure(plain_failure(
                format!("{purpose} のブランチを作業用 worktree に用意するため"),
                format!("flock {}", self.slot_dir.display()),
                format!(
                    "{purpose} を {} で作業できる状態にしてから作業して",
                    self.slot_dir.display()
                ),
                &anyhow!("the per-PR worktree lock is not held, so ghost will not check the branch out in the main clone"),
            ));
            return;
        }
        let outcome = workdir::sync_and_checkout(
            &self.main_clone,
            &self.slot_dir,
            &self.slug,
            checkout,
            PromptMode::Batch,
            purpose,
        );
        self.apply_checkout(outcome);
    }

    fn apply_checkout(&mut self, outcome: workdir::CheckoutOutcome) {
        if !outcome.note.is_empty() {
            self.log(outcome.note);
        }
        self.workdir = outcome.workdir.clone();
        self.agent.set_workdir(outcome.workdir);
        for failure in outcome.failures {
            self.push_setup_failure(failure);
        }
    }

    fn decorate(&mut self, task: String) -> String {
        if self.setup_failures.is_empty() {
            return task;
        }
        self.setup_handed = true;
        prompt::render_setup_handoff(&self.setup_failures, &task)
    }

    /// One agent turn for a setup failure when the poll itself has nothing to do.
    /// Returns true when that turn was started.
    fn delegate_setup(&mut self) -> Result<bool> {
        if self.setup_failures.is_empty() || self.setup_handed {
            return Ok(false);
        }
        let task = self.decorate(
            "目指す状態に到達したら、このターンはそこで終えてよい。到達できない理由があれば報告する。"
                .into(),
        );
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        Ok(true)
    }

    fn note_setup_result(&mut self, ok: bool) {
        if ok {
            self.setup_failures.clear();
            self.setup_handed = false;
        }
    }

    fn push_setup_failure(&mut self, failure: SetupFailure) {
        if self.setup_failures.iter().any(|have| {
            have.command == failure.command
                && have.exit_code == failure.exit_code
                && have.stderr == failure.stderr
        }) {
            return;
        }
        self.log_kind(LogKind::Error, prompt::setup_warning(&failure));
        self.setup_failures.push(failure);
        self.setup_handed = false;
    }

    fn load_diff(&mut self, number: u64, last: Option<&str>, head: &str) -> Result<String> {
        if let Some(last) = last {
            if !last.is_empty() && last != head {
                if workdir::has_commit(&self.workdir, last)
                    && workdir::has_commit(&self.workdir, head)
                {
                    self.log(format!("diff {}..{}", short_sha(last), short_sha(head)));
                    return workdir::diff_range(&self.workdir, last, head);
                }
                self.log("previous SHA is not in the clone; sending the whole PR diff");
            }
        }
        self.log(format!("gh pr diff {number} --repo {}", self.slug));
        self.gh.pr_diff(number)
    }

    /// `Ok(true)` only after a completed turn with exit 0 (or a live clean end).
    /// A failed resume is retried once as a fresh session with the same prompt.
    fn dispatch_task(&mut self, prompt: &str) -> Result<bool> {
        if self.quit {
            return Ok(false);
        }
        let mut allow_fresh = true;
        loop {
            if self.turns >= self.spec.max_turns {
                self.stop(3, self.max_message());
                return Ok(false);
            }
            let launch = self.agent.dispatch(prompt)?;
            self.turns += 1;
            self.interrupted = false;
            self.log(describe_launch(self.agent.kind(), &launch));
            self.refresh_session_status();
            self.status.phase = "running".into();
            self.status.turns = format!("{}/{}", self.turns, self.spec.max_turns);
            self.push_status();
            self.wait_turn()?;
            if self.quit {
                return Ok(false);
            }
            if !counts_as_agent_turn(
                &launch,
                self.agent.last_exit_code(),
                self.agent.recent_output(),
            ) {
                self.turns = self.turns.saturating_sub(1);
                self.status.turns = format!("{}/{}", self.turns, self.spec.max_turns);
                self.log(self.say(
                    "resume の起動引数が拒否されたため、この試行はターンに数えません",
                    "resume rejected its arguments, so this attempt does not count as a turn",
                ));
            } else {
                self.log(format!(
                    "agent turn finished ({}/{})",
                    self.turns, self.spec.max_turns
                ));
            }
            self.refresh_session_status();
            self.push_status();
            let ok = record_progress(
                self.interrupted,
                self.agent.last_exit_code(),
                self.agent.clean_live(),
                self.agent.saw_error(),
            );
            if ok {
                self.fail_streak = 0;
                return Ok(true);
            }
            if self.interrupted {
                return Ok(false);
            }
            let usage = !counts_as_agent_turn(
                &launch,
                self.agent.last_exit_code(),
                self.agent.recent_output(),
            );
            if allow_fresh && matches!(launch, Launch::Resume { .. }) {
                self.log(self.say(
                    "resume に失敗したので、同じ文脈で新しいセッションを開始します",
                    "resume failed; starting a fresh session with the same context",
                ));
                self.agent.forget_session();
                allow_fresh = false;
                if !usage {
                    self.backoff_after_failure()?;
                    if self.quit {
                        return Ok(false);
                    }
                }
                continue;
            }
            let code = self
                .agent
                .last_exit_code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".into());
            self.log(self.say(
                &format!("エージェントのターンが失敗しました（exit {code}）"),
                &format!("agent turn failed (exit {code})"),
            ));
            if !usage {
                self.backoff_after_failure()?;
            }
            return Ok(false);
        }
    }

    /// Wait longer after each real failure, and stop after a short streak
    /// so a broken agent cannot spend the whole turn budget in one poll cycle.
    fn backoff_after_failure(&mut self) -> Result<()> {
        self.fail_streak += 1;
        if stop_after_failures(self.fail_streak) {
            let n = MAX_CONSECUTIVE_AGENT_FAILURES;
            self.stop(
                1,
                self.say(
                    &format!(
                        "エージェントが {n} 回連続で失敗したので停止します。ログを確認してから再実行してください。"
                    ),
                    &format!(
                        "Stopped after {n} consecutive agent failures. Check the log, then re-run ghost."
                    ),
                ),
            );
            return Ok(());
        }
        let wait = agent_backoff(self.fail_streak);
        let left = format_countdown(wait);
        let n = MAX_CONSECUTIVE_AGENT_FAILURES;
        self.log(self.say(
            &format!(
                "エージェントが失敗しました（連続 {}/{}）。{left} 待ってから次を試します。",
                self.fail_streak, n
            ),
            &format!(
                "agent failed ({}/{} in a row); waiting {left} before the next attempt",
                self.fail_streak, n
            ),
        ));
        self.wait_for(wait)
    }

    fn wait_for(&mut self, total: Duration) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < total {
            if self.quit {
                return Ok(());
            }
            let left = total.saturating_sub(start.elapsed());
            self.status.phase = format!("backoff {}", format_countdown(left));
            self.push_status();
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiCmd::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(cmd) => self.handle_cmd(cmd)?,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        Ok(())
    }

    fn wait_turn(&mut self) -> Result<()> {
        loop {
            if self.quit {
                return Ok(());
            }
            for line in self.agent.poll_lines() {
                for part in line.lines() {
                    if !part.trim().is_empty() {
                        self.log_kind(LogKind::Agent, truncate(part, 2_000));
                        if let Some(hint) = shell::credential_hint(part) {
                            self.log_kind(LogKind::Error, hint);
                        }
                    }
                }
            }
            if self.agent.turn_finished() {
                if !self.quit && !self.interrupted {
                    self.flush_queued()?;
                }
                return Ok(());
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(UiCmd::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    self.agent.stop();
                    self.quit = true;
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(UiCmd::Interrupt) => {
                    self.agent.interrupt();
                    self.interrupted = true;
                    self.log("interrupted the agent; the session stays open");
                }
                Ok(UiCmd::Input(text)) => self.on_input(text, true)?,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Returns true when the caller should leave the watch loop.
    fn wait_interval(&mut self) -> Result<bool> {
        if self.quit {
            return Ok(true);
        }
        let start = Instant::now();
        loop {
            if self.quit {
                return Ok(true);
            }
            if start.elapsed() >= self.spec.interval {
                return Ok(false);
            }
            let left = self.spec.interval.saturating_sub(start.elapsed());
            self.status.phase = format!("next {}", format_countdown(left));
            self.push_status();
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiCmd::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    self.stop(0, "quit".into());
                    return Ok(true);
                }
                Ok(cmd) => self.handle_cmd(cmd)?,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn wait_confirm(&mut self) -> Result<Option<bool>> {
        loop {
            match self.rx.recv() {
                Ok(UiCmd::Confirm(answer)) => return Ok(Some(answer)),
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(None);
                }
                Ok(UiCmd::Input(text)) => self.on_input(text, false)?,
                Ok(UiCmd::Interrupt) => self.agent.interrupt(),
                Ok(_) => {}
            }
        }
    }

    fn handle_cmd(&mut self, cmd: UiCmd) -> Result<()> {
        match cmd {
            UiCmd::Quit => self.stop(0, "quit".into()),
            UiCmd::Interrupt => {
                self.agent.interrupt();
                self.interrupted = true;
                self.log("interrupted the agent; the session stays open");
            }
            UiCmd::Input(text) => self.on_input(text, self.agent.turn_open())?,
            UiCmd::Pick { .. } | UiCmd::Choose(_) | UiCmd::Confirm(_) => {}
        }
        Ok(())
    }

    fn on_input(&mut self, text: String, during_turn: bool) -> Result<()> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.log_kind(LogKind::User, text.clone());
        if during_turn && self.agent.accepts_live_input() {
            self.agent.push_live(&text)?;
            self.log("sent into the running session");
            return Ok(());
        }
        if during_turn {
            self.queued.push(text);
            self.log("queued; it will be sent into this same session when the turn finishes");
            return Ok(());
        }
        if self.agent.session_id().is_some() {
            let prompt = self.decorate(prompt::user_nudge(self.spec.lang, &text, false));
            let ok = self.dispatch_task(&prompt)?;
            self.note_setup_result(ok);
        } else {
            self.queued.push(text);
            self.log("queued for the first task");
        }
        Ok(())
    }

    /// Codex and Grok cannot take stdin mid-turn. Send those lines as one
    /// follow-up in the session that just finished, instead of dropping them.
    fn flush_queued(&mut self) -> Result<()> {
        if self.queued.is_empty() || self.agent.session_id().is_none() {
            return Ok(());
        }
        let notes = std::mem::take(&mut self.queued);
        let prompt = self.decorate(prompt::user_nudge(
            self.spec.lang,
            &notes.join("\n\n"),
            true,
        ));
        let ok = self.dispatch_task(&prompt)?;
        self.note_setup_result(ok);
        Ok(())
    }

    fn find_pr_after(&mut self, issue: u64, branch: &str) -> Result<Option<u64>> {
        for attempt in 0..3 {
            if let Some(number) = self.gh.find_issue_pr(issue, branch)? {
                return Ok(Some(number));
            }
            if attempt == 2 {
                break;
            }
            self.log("pull request not visible yet; checking gh again");
            if self.pause(Duration::from_secs(2)) {
                self.stop(0, "quit".into());
                return Ok(None);
            }
        }
        Err(anyhow!(
            "the agent finished but no pull request for issue #{issue} was found. Expected branch {branch} or a body containing Closes #{issue}."
        ))
    }

    fn pause(&mut self, dur: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < dur {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UiCmd::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    self.quit = true;
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn remember(&mut self, stored: &mut GhostState, slot: SessionSlot) {
        if let Some(id) = self.agent.session_id() {
            stored.remember_session(slot, self.agent.kind(), id);
        }
    }

    fn refresh_session_status(&mut self) {
        self.status.session = self
            .agent
            .session_id()
            .map(|id| short_id(id).to_string())
            .unwrap_or_else(|| "—".into());
        self.status.agent = self.agent.kind().to_string();
        self.status.turns = format!("{}/{}", self.turns, self.spec.max_turns);
    }

    fn stop(&mut self, code: i32, message: String) {
        if !message.is_empty() && message != "quit" {
            self.log(message.clone());
        }
        self.agent.stop();
        self.quit = true;
        let _ = self.tx.send(UiEvent::Exit { code, message });
    }

    fn max_message(&self) -> String {
        let n = self.spec.max_turns;
        self.say(
            &format!(
                "AI ターンが上限の {n} に達しました。PR はまだマージ可能ではありません。--review と --code が永久に往復しないよう停止します。状態は保存済みです。続けるには ghost を再実行してください。"
            ),
            &format!(
                "Hit the max of {n} AI turns without the PR becoming merge-ready. Stopping so --review and --code cannot ping-pong forever. State was saved; re-run ghost to continue."
            ),
        )
    }

    fn say(&self, ja: &str, en: &str) -> String {
        self.spec.lang.say(ja, en)
    }

    fn login(&self) -> String {
        self.login.clone().unwrap_or_default()
    }

    fn use_slot(&mut self, slot: &str) {
        let Some(ghost_home) = self
            .main_clone
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
        else {
            self.push_setup_failure(plain_failure(
                format!("{slot} の作業ディレクトリを決めるため"),
                "resolve ghost home",
                format!("{slot} の worktree パスが分かっている状態にしてから作業して"),
                &anyhow!("work directory has no home"),
            ));
            return;
        };
        let dest = workdir::slot_path(&ghost_home, &self.owner, &self.repo, slot);
        match workdir::lock_slot(&dest) {
            Ok(lock) => {
                self.slot_lock = Some(lock);
                self.slot_dir = dest.clone();
                let actual = workdir::best_workdir(&dest, &self.main_clone, &ghost_home);
                self.workdir = actual.clone();
                self.agent.set_workdir(actual);
            }
            Err(err) => {
                self.push_setup_failure(plain_failure(
                    format!("{slot} の作業ディレクトリを排他で使うため"),
                    format!("flock {}", dest.display()),
                    format!(
                        "{} をこのプロセスだけで使える状態にしてから作業して",
                        dest.display()
                    ),
                    &err,
                ));
            }
        }
    }

    /// Returns a poll line when comments were dropped, such as
    /// `filtered 3 (untrusted author)`.
    fn prepare_notes(&self, snap: &mut PrSnap, agent_ids: &BTreeSet<String>) -> Option<String> {
        let login = self.login();
        mark_viewer(&mut snap.notes, &login);
        let filtered = filter_notes(
            &snap.notes,
            self.spec.trust,
            &login,
            &snap.author,
            &self.owner,
            agent_ids,
        );
        let line = filtered.stats.log_line();
        snap.notes = filtered.kept;
        line
    }

    fn push_status(&mut self) {
        let _ = self.tx.send(UiEvent::Status(self.status.clone()));
    }

    fn log(&mut self, text: impl Into<String>) {
        self.log_kind(LogKind::System, text);
    }

    fn log_kind(&mut self, kind: LogKind, text: impl Into<String>) {
        log(&self.tx, kind, text);
    }
}

fn log(tx: &Sender<UiEvent>, kind: LogKind, text: impl Into<String>) {
    let _ = tx.send(UiEvent::Log(LogLine {
        kind,
        text: text.into(),
    }));
}

/// Repo-only mode with nothing open prints this and exits 0 instead of the picker.
fn empty_repo_notice(slug: &str, empty: bool, lang: Lang) -> Option<String> {
    if empty {
        Some(lang.say(
            &format!("{slug} にオープンな issue も PR もありません。"),
            &format!("No open issues or PRs in {slug}."),
        ))
    } else {
        None
    }
}

/// List open PRs and issues before the TUI. An empty repo skips the picker.
/// Preflight or `gh` failures fall through so the TUI can show them.
fn skip_empty_picker(spec: &RunSpec) -> Result<Option<String>> {
    if !matches!(
        classify_startup(&spec.target.slug(), spec.lang),
        Startup::Continue { failure: None, .. }
    ) {
        return Ok(None);
    }
    let items = match Gh::new(spec.target.slug()).open_items() {
        Ok(items) => items,
        Err(_) => return Ok(None),
    };
    Ok(empty_repo_notice(
        &spec.target.slug(),
        items.is_empty(),
        spec.lang,
    ))
}

enum Startup {
    Fatal(String),
    Continue {
        login: Option<String>,
        failure: Option<SetupFailure>,
    },
}

fn classify_startup(slug: &str, lang: Lang) -> Startup {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = which("gh", &path).is_some();
    let auth_out = if found { Gh::auth_probe().ok() } else { None };
    let auth = AuthProbe {
        found,
        exit_code: auth_out.as_ref().map(|o| o.code),
        stdout: auth_out
            .as_ref()
            .map(|o| o.stdout.clone())
            .unwrap_or_default(),
        stderr: auth_out
            .as_ref()
            .map(|o| o.stderr.clone())
            .unwrap_or_default(),
    };
    let logged_in = found
        && auth.exit_code == Some(0)
        && format!("{}\n{}", auth.stdout, auth.stderr).contains("Logged in");
    let repo = if logged_in {
        let gh = Gh::new(slug);
        match gh.repo_probe() {
            Ok(out) => Some(RepoProbe {
                exit_code: out.code,
                stdout: out.stdout,
                stderr: out.stderr,
            }),
            Err(err) => Some(RepoProbe {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("{err:#}"),
            }),
        }
    } else {
        None
    };
    match evaluate(&auth, repo.as_ref(), slug) {
        Ok(ok) => Startup::Continue {
            login: ok.login,
            failure: None,
        },
        Err(kind) if is_fatal(&kind) => {
            Startup::Fatal(failure_message(&kind, &auth, repo.as_ref(), slug, lang))
        }
        Err(_) => {
            let probe = repo.unwrap_or(RepoProbe {
                exit_code: 1,
                stdout: String::new(),
                stderr: "repository probe was not run".into(),
            });
            let detail = if probe.stderr.trim().is_empty() {
                probe.stdout
            } else {
                probe.stderr
            };
            Startup::Continue {
                login: crate::preflight::parse_login(&format!("{}\n{}", auth.stdout, auth.stderr)),
                failure: Some(SetupFailure::from_step(
                    SetupStep {
                        intent: format!("{slug} にアクセスできるか確認するため"),
                        command: format!("gh repo view {slug} --json nameWithOwner"),
                        desired_state: format!("{slug} を gh で読める状態にしてから作業して"),
                    },
                    probe.exit_code,
                    detail,
                )),
            }
        }
    }
}

fn pr_purpose(mode: RunMode, number: u64) -> String {
    match mode {
        RunMode::Review => format!("レビュー対象の PR #{number}"),
        RunMode::Code => format!("修正対象の PR #{number}"),
        RunMode::Issue => format!("issue から続く PR #{number}"),
        RunMode::Ask => format!("対象の PR #{number}"),
        RunMode::Picker => format!("選択された PR #{number}"),
    }
}

fn gh_failure(
    intent: impl Into<String>,
    command: impl Into<String>,
    desired: impl Into<String>,
    err: &anyhow::Error,
) -> SetupFailure {
    SetupFailure::from_step(
        SetupStep {
            intent: intent.into(),
            command: command.into(),
            desired_state: desired.into(),
        },
        1,
        format!("{err:#}"),
    )
}

fn plain_failure(
    intent: impl Into<String>,
    command: impl Into<String>,
    desired: impl Into<String>,
    err: &anyhow::Error,
) -> SetupFailure {
    gh_failure(intent, command, desired, err)
}

fn failed_checkout(
    main: &std::path::Path,
    dest: &std::path::Path,
    failure: SetupFailure,
) -> workdir::CheckoutOutcome {
    let home = main
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| main.to_path_buf());
    workdir::CheckoutOutcome {
        workdir: workdir::best_workdir(dest, main, &home),
        note: String::new(),
        failures: vec![failure],
    }
}

fn wait_quit(rx: &Receiver<UiCmd>) {
    loop {
        match rx.recv() {
            Ok(UiCmd::Quit) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn pull_number(target: &GhTarget) -> Result<u64> {
    match target {
        GhTarget::Pull { number, .. } => Ok(*number),
        _ => Err(anyhow!("expected a pull request URL")),
    }
}

fn issue_number(target: &GhTarget) -> Result<u64> {
    match target {
        GhTarget::Issue { number, .. } => Ok(*number),
        _ => Err(anyhow!("expected an issue URL")),
    }
}

fn mode_name(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Review => "review",
        RunMode::Code => "code",
        RunMode::Issue => "issue",
        RunMode::Picker => "picker",
        RunMode::Ask => "pr",
    }
}

/// Real failures in a row before ghost stops instead of retrying.
pub const MAX_CONSECUTIVE_AGENT_FAILURES: u32 = 5;

/// 20s, 40s, 80s, … capped at 10 minutes.
pub fn agent_backoff(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(5);
    Duration::from_secs((20u64 << exp).min(10 * 60))
}

pub fn stop_after_failures(streak: u32) -> bool {
    streak >= MAX_CONSECUTIVE_AGENT_FAILURES
}

fn label_refs(labels: &[String]) -> Vec<&str> {
    labels.iter().map(String::as_str).collect()
}

fn short_id(id: &str) -> &str {
    let n = id.len().min(8);
    &id[..n]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        agent_backoff, empty_repo_notice, stop_after_failures, MAX_CONSECUTIVE_AGENT_FAILURES,
    };
    use crate::lang::Lang;

    #[test]
    fn agent_failure_backoff_grows_and_then_stops() {
        assert_eq!(agent_backoff(1), Duration::from_secs(20));
        assert_eq!(agent_backoff(2), Duration::from_secs(40));
        assert_eq!(agent_backoff(3), Duration::from_secs(80));
        assert_eq!(agent_backoff(4), Duration::from_secs(160));
        assert!(agent_backoff(20) <= Duration::from_secs(10 * 60));
        assert!(!stop_after_failures(MAX_CONSECUTIVE_AGENT_FAILURES - 1));
        assert!(stop_after_failures(MAX_CONSECUTIVE_AGENT_FAILURES));
    }

    #[test]
    fn empty_repo_skips_the_picker() {
        assert_eq!(
            empty_repo_notice("OWNER/REPO", true, Lang::En).as_deref(),
            Some("No open issues or PRs in OWNER/REPO.")
        );
        assert_eq!(
            empty_repo_notice("OWNER/REPO", true, Lang::Ja).as_deref(),
            Some("OWNER/REPO にオープンな issue も PR もありません。")
        );
        assert!(empty_repo_notice("OWNER/REPO", false, Lang::Ja).is_none());
    }
}

fn ci_handled(stored: &GhostState) -> Option<CiHandled> {
    let sha = stored.handled_ci_sha.clone()?;
    Some(CiHandled::new(sha, stored.handled_failing_checks.clone()))
}
