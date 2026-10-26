//! Poll GitHub with `gh`. Review, code, and issue each start the agent on
//! launch for a situation check, even when saved state says nothing is new.
//! Later polls talk to that same session only when something changed.
//! Text typed with `i` is queued during a turn and sent when it ends, or
//! starts a session when none is alive.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::agent::{
    agent_on_path, counts_as_agent_turn, describe_launch, pick_agent, record_progress,
    resume_unavailable, AgentKind, AgentSession, Launch, PushPlan,
};
use crate::github::Gh;
use crate::lang::Lang;
use crate::merge::{
    accept_survey_refetch, auto_merge_answer, commit_merge, is_merge_ready, merge_method_rejected,
    merge_prompt_again, merge_ready_line, MergeAnswer, MergeHold, MergeReadyStreak, StopReason,
};
use crate::preflight::{evaluate, failure_message, is_fatal, AuthProbe, RepoProbe};
use crate::prompt::{self, truncate};
use crate::setup::{SetupFailure, SetupStep};
use crate::shell::{self, which, PromptMode};
use crate::state::{self, GhostState, SessionSlot};
use crate::target::GhTarget;
use crate::text::{format_countdown, short_sha};
use crate::tui::{
    self, merge_question, ConfirmAnswer, ConfirmKind, LogKind, LogLine, MergeStyle, Side, Status,
    UiCmd, UiEvent,
};
use crate::watch::{
    after_review_verdict, approve_signal, baseline_agent_comments, code_step, decide_code,
    decide_review, ensure_closing_line, feedback_log, filter_notes, mark_viewer, review_step,
    seen_after_survey, CiHandled, CodeAction, CodeLaunch, CodeStep, CommentTrust, MaxTurns, Note,
    PrSnap, ReviewAction, ReviewLaunch, ReviewSignal, ReviewStep,
};
use crate::workdir::{self, Checkout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Review,
    Code,
    Issue,
    Picker,
    /// Open issues only. No pull requests.
    IssueList,
    /// Open pull requests. `--review` or `--code` is chosen before the picker.
    PullList(ListedPull),
    /// Take the oldest eligible issue, then check again.
    Auto,
    Ask,
}

/// Which loop a `/pulls` pick starts. The flag is required up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedPull {
    Review,
    Code,
}

pub struct RunSpec {
    pub header: String,
    pub target: GhTarget,
    pub mode: RunMode,
    pub interval: Duration,
    pub agent: Option<AgentKind>,
    /// Auto-answer yes on the merge prompt. Ignored by `--review`.
    pub merge_yes: bool,
    /// Exit successfully instead of asking on the merge prompt.
    pub merge_no: bool,
    pub merge_method: Option<String>,
    pub max_turns: u32,
    pub lang: Lang,
    pub ssh_key: Option<String>,
    /// Who may wake `--code`. Default `all`, including issue and `--code` review comments.
    pub trust: CommentTrust,
    /// Which issue authors `--auto` may run. Default collaborators.
    pub trust_issues: CommentTrust,
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
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            babysit(
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
            )
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let message = format!("{err:#}");
                let _ = tx.send(UiEvent::Fatal(message.clone()));
                let _ = tx.send(UiEvent::Exit { code: 1, message });
            }
            Err(payload) => {
                let message = panic_message(&payload);
                let _ = tx.send(UiEvent::Fatal(message.clone()));
                let _ = tx.send(UiEvent::Exit { code: 1, message });
            }
        }
    })
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    let text = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".into()
    };
    format!("worker panicked: {text}")
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
        RunMode::Picker | RunMode::IssueList | RunMode::PullList(_) | RunMode::Auto => {
            workdir::CheckoutOutcome {
                workdir: dest.to_path_buf(),
                note: String::new(),
                failures: Vec::new(),
            }
        }
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
    let slot = match spec.mode {
        RunMode::Picker
        | RunMode::IssueList
        | RunMode::PullList(_)
        | RunMode::Auto
        | RunMode::Ask => None,
        RunMode::Issue => match issue_number(&spec.target) {
            Ok(number) => Some(format!("issue-{number}")),
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
        RunMode::Review => match pull_number(&spec.target) {
            Ok(number) => Some(state::pr_slot_name(number, SessionSlot::Review)),
            Err(err) => {
                return slot_parse_failure(main, err);
            }
        },
        RunMode::Code => match pull_number(&spec.target) {
            Ok(number) => Some(state::pr_slot_name(number, SessionSlot::Code)),
            Err(err) => {
                return slot_parse_failure(main, err);
            }
        },
    };
    let Some(slot) = slot else {
        return SlotPrep {
            intended: main.to_path_buf(),
            workdir: main.to_path_buf(),
            lock: None,
            failures: Vec::new(),
        };
    };
    let dest = workdir::slot_path(home, spec.target.owner(), spec.target.repo(), &slot);
    match workdir::lock_slot(&dest) {
        Ok(lock) => SlotPrep {
            workdir: dest.clone(),
            intended: dest,
            lock: Some(lock),
            failures: Vec::new(),
        },
        Err(err) => SlotPrep {
            // A failed lock must not fall back to the main clone.
            intended: dest.clone(),
            workdir: dest.clone(),
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

fn slot_parse_failure(main: &std::path::Path, err: anyhow::Error) -> SlotPrep {
    SlotPrep {
        intended: main.to_path_buf(),
        workdir: main.to_path_buf(),
        lock: None,
        failures: vec![plain_failure(
            "対象の番号を読むため",
            "parse pull request url",
            "PR 番号が分かっている状態にしてから作業して",
            &err,
        )],
    }
}

#[allow(clippy::too_many_arguments)]
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

    let interval = spec.interval;
    let slot_name = slot_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("main");
    let kind = pick_agent(spec.agent, agent_on_path).map_err(|e| anyhow!(e))?;
    status.agent = kind.to_string();
    let _ = tx.send(UiEvent::Status(status.clone()));
    log(&tx, LogKind::System, format!("agent {kind}"));

    let slug = spec.target.slug();
    let owner = spec.target.owner().to_string();
    let repo = spec.target.repo().to_string();
    let state_root = state::state_dir();
    let mut agent = AgentSession::new(kind, workdir.clone(), &home, slot_name);
    agent.set_created_issues(state::created_issues_path(&state_root, &owner, &repo));
    agent.set_created_prs(state::created_prs_path(&state_root, &owner, &repo));
    agent.set_repo_slug(slug.clone());
    let mut ctx = Ctx {
        gh: Gh::new(slug.clone()),
        slug,
        owner,
        repo,
        agent,
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
        delivering_queue: false,
        turn_signal: String::new(),
        status,
        quit: false,
        interrupted: false,
        last_idle_sha: None,
        merge_streak: MergeReadyStreak::with_gap(interval),
        merge_hold: None,
        cwd_fallback_logged: false,
        code_pr: None,
        code_path: None,
        code_seen: BTreeSet::new(),
        code_agent_ids: BTreeSet::new(),
        state_root,
        auto_issue: None,
        auto_release: None,
    };

    match ctx.spec.mode {
        RunMode::Review => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.review_loop(n)?;
        }
        RunMode::Code => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.code_loop(n, CodeLaunch::Survey)?;
        }
        RunMode::Issue => {
            let n = issue_number(&ctx.spec.target)?;
            ctx.issue_flow(n)?;
        }
        RunMode::Ask => {
            let n = pull_number(&ctx.spec.target)?;
            ctx.ask_mode(n)?;
        }
        RunMode::Picker => ctx.picker_flow(false)?,
        RunMode::IssueList => ctx.picker_flow(true)?,
        RunMode::PullList(side) => ctx.pulls_flow(side)?,
        RunMode::Auto => ctx.auto_loop()?,
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
    /// Set while a queued follow-up is being delivered, so that turn does not
    /// flush the queue again or replace the situation-check signal.
    delivering_queue: bool,
    /// Agent output from the turn that just finished, before any queued follow-up.
    turn_signal: String,
    status: Status,
    quit: bool,
    interrupted: bool,
    last_idle_sha: Option<String>,
    merge_streak: MergeReadyStreak,
    merge_hold: Option<MergeHold>,
    cwd_fallback_logged: bool,
    code_pr: Option<u64>,
    code_path: Option<PathBuf>,
    code_seen: BTreeSet<String>,
    code_agent_ids: BTreeSet<String>,
    state_root: PathBuf,
    /// Set while `--auto` is inside one issue, so `stop` returns to the loop.
    auto_issue: Option<u64>,
    auto_release: Option<AutoRelease>,
}

impl Ctx {
    fn review_loop(&mut self, number: u64) -> Result<()> {
        self.status.mode = "review".into();
        self.status.target = format!("PR #{number}");
        let path = state::pr_state_path(
            &self.state_root,
            &self.owner,
            &self.repo,
            number,
            SessionSlot::Review,
        );
        let mut stored = GhostState::load(&path)?;
        self.arm_review_push();
        // Launch starts a new session. A saved id is not resumed.
        self.push_status();
        let mut opened = false;
        loop {
            if self.quit {
                return Ok(());
            }
            let snap = match self.poll_pr(number)? {
                Some(snap) => snap,
                None => return Ok(()),
            };
            if snap.comments_unavailable && opened {
                self.log(self.say(
                    "コメントの取得に失敗したので、この poll は飛ばします",
                    "comment endpoints failed; skipping this poll",
                ));
                if self.wait_interval()? {
                    return Ok(());
                }
                continue;
            }
            let action = match decide_review(
                &snap,
                stored.last_reviewed_sha.as_deref(),
                self.turns,
                self.spec.max_turns,
                ReviewSignal::default(),
            ) {
                Err(MaxTurns) => {
                    self.stop(3, self.max_message());
                    return Ok(());
                }
                Ok(action) => action,
            };
            let opening = !opened;
            let launch = if opening {
                ReviewLaunch::Opening
            } else {
                ReviewLaunch::Watching
            };
            match review_step(launch, opened, action) {
                ReviewStep::Action(ReviewAction::Stop(reason)) => {
                    self.stop(reason.exit_code(), reason.message(self.spec.lang, number));
                    return Ok(());
                }
                ReviewStep::Action(ReviewAction::Idle) => {
                    if !self.delegate_setup()? {
                        self.note_idle(&snap.head_sha);
                    }
                }
                ReviewStep::Turn | ReviewStep::Action(ReviewAction::Review { .. }) => {
                    let (leave, progressed) =
                        self.run_review_turn(number, &snap, &path, &mut stored, opening)?;
                    if leave {
                        return Ok(());
                    }
                    if progressed {
                        opened = true;
                    }
                }
            }
            if self.wait_interval()? {
                return Ok(());
            }
        }
    }

    /// One review turn. `(leave, progressed)`: leave ends the process, progressed
    /// means this head was reviewed so later polls may idle on the same SHA.
    fn run_review_turn(
        &mut self,
        number: u64,
        snap: &PrSnap,
        path: &std::path::Path,
        stored: &mut GhostState,
        opening: bool,
    ) -> Result<(bool, bool)> {
        if opening {
            self.log_situation();
        }
        self.checkout(
            Checkout::Pr {
                number,
                branch: Some(snap.head_branch.clone()),
            },
            &pr_purpose(RunMode::Review, number),
        );
        if self.quit {
            return Ok((true, false));
        }
        let previous = if opening {
            None
        } else {
            stored.last_reviewed_sha.as_deref()
        };
        let diff = self.load_diff(number, previous, &snap.head_sha)?;
        let notes = self.take_queued_for_prompt();
        let task = self.decorate(if opening || self.agent.session_id().is_none() {
            prompt::review_initial(
                &self.slug,
                number,
                &snap.title,
                &snap.head_sha,
                &self.workdir,
                &diff,
                &notes,
                self.spec.lang,
            )
        } else {
            prompt::review_followup(
                &self.slug,
                number,
                &snap.title,
                previous.unwrap_or("unreviewed"),
                &snap.head_sha,
                &self.workdir,
                &diff,
                &notes,
                self.spec.lang,
            )
        });
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        if !ok {
            self.requeue_after_failed_send(notes);
        }
        if self.quit {
            return Ok((true, false));
        }
        if !ok {
            self.log(self.say(
                "エージェントのターンが失敗したので、レビュー済み SHA は記録しません",
                "agent turn failed; not recording the reviewed SHA",
            ));
            return Ok((false, false));
        }
        stored.last_reviewed_sha = Some(snap.head_sha.clone());
        self.remember(stored, SessionSlot::Review);
        stored.save(path)?;
        self.log(format!(
            "recorded reviewed SHA {}",
            short_sha(&snap.head_sha)
        ));
        let approve = approve_signal(&self.turn_signal);
        if after_review_verdict(approve) == crate::watch::AfterVerdict::Exit {
            self.stop(
                0,
                StopReason::ReviewApproved.message(self.spec.lang, number),
            );
            return Ok((true, true));
        }
        Ok((false, true))
    }

    fn code_loop(&mut self, number: u64, launch: CodeLaunch) -> Result<()> {
        self.status.mode = "code".into();
        self.status.target = format!("PR #{number}");
        let path = state::pr_state_path(
            &self.state_root,
            &self.owner,
            &self.repo,
            number,
            SessionSlot::Code,
        );
        let mut stored = GhostState::load(&path)?;
        // Do not resume a saved code session. Issue handoff already holds its id
        // in this process; a fresh `--code` launch starts a new session.
        self.code_pr = Some(number);
        self.code_path = Some(path.clone());
        self.code_seen = stored.seen_comment_ids.iter().cloned().collect();
        self.code_agent_ids = stored.agent_comment_ids.iter().cloned().collect();
        let mut surveyed = matches!(launch, CodeLaunch::Handoff { .. });
        let mut created_here = matches!(launch, CodeLaunch::Handoff { created_here: true });
        self.push_status();
        loop {
            if self.quit {
                return Ok(());
            }
            let (snap, merge_confirmed) = match self.poll_pr(number)? {
                Some(mut snap) => {
                    let agent_ids = self.code_agent_ids.clone();
                    let filtered = self.prepare_notes(&mut snap, &agent_ids);
                    self.log(filtered);
                    self.log(merge_ready_line(&snap.merge));
                    let merge_confirmed = self
                        .merge_streak
                        .observe(&snap.head_sha, is_merge_ready(&snap.merge));
                    if is_merge_ready(&snap.merge) && !merge_confirmed {
                        self.log_merge_wait(&snap.head_sha);
                    }
                    (snap, merge_confirmed)
                }
                None => return Ok(()),
            };
            if snap.comments_unavailable && surveyed {
                self.log(self.say(
                    "コメントの取得に失敗したので、この poll は飛ばします",
                    "comment endpoints failed; skipping this poll",
                ));
                if self.wait_interval()? {
                    return Ok(());
                }
                continue;
            }
            let handled = ci_handled(&stored);
            let seen = self.code_seen.clone();
            let action = match decide_code(
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
                Ok(action) => action,
            };
            match code_step(launch, surveyed, action) {
                CodeStep::Action(CodeAction::Stop(reason)) => {
                    if reason == StopReason::MergeReady {
                        if merge_confirmed && self.offer_merge(number, &snap)? {
                            return Ok(());
                        }
                    } else {
                        self.stop(reason.exit_code(), reason.message(self.spec.lang, number));
                        return Ok(());
                    }
                }
                CodeStep::Action(CodeAction::Idle) => {
                    if !self.delegate_setup()? {
                        self.note_code_idle(&snap);
                    }
                }
                CodeStep::Action(CodeAction::Fix {
                    ci_failed,
                    new_notes,
                }) => {
                    self.run_code_fix(
                        number,
                        &snap,
                        &path,
                        &mut stored,
                        ci_failed,
                        new_notes,
                        created_here,
                    )?;
                    created_here = false;
                    if self.quit {
                        return Ok(());
                    }
                }
                CodeStep::Survey => {
                    let (leave, progressed) =
                        self.run_code_survey(number, &snap, &path, &mut stored)?;
                    if leave {
                        return Ok(());
                    }
                    if progressed {
                        surveyed = true;
                    }
                }
            }
            if self.wait_interval()? {
                return Ok(());
            }
        }
    }

    fn run_code_survey(
        &mut self,
        number: u64,
        snap: &PrSnap,
        path: &std::path::Path,
        stored: &mut GhostState,
    ) -> Result<(bool, bool)> {
        self.log_situation();
        self.checkout(
            Checkout::Pr {
                number,
                branch: Some(snap.head_branch.clone()),
            },
            &pr_purpose(RunMode::Code, number),
        );
        if self.quit {
            return Ok((true, false));
        }
        let startup_ids: Vec<String> = snap.notes.iter().map(|n| n.id.clone()).collect();
        let user_notes = self.take_queued_for_prompt();
        let ci_failed = snap.merge.checks == crate::merge::ChecksVerdict::Failed;
        let push = self.arm_code_push(snap);
        let task = self.decorate(prompt::code_survey(
            &self.slug,
            number,
            &snap.head_branch,
            &snap.head_sha,
            &self.workdir,
            ci_failed,
            &snap.failing_checks,
            &snap.notes,
            &user_notes,
            self.spec.lang,
            &push,
        ));
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        if !ok {
            self.requeue_after_failed_send(user_notes);
        }
        if self.quit {
            return Ok((true, false));
        }
        if !ok {
            self.log(self.say(
                "エージェントのターンが失敗したので、起動時のコメントは既読にしません",
                "agent turn failed; not marking the startup comments as seen",
            ));
            return Ok((false, false));
        }
        for id in seen_after_survey(&startup_ids, true) {
            self.code_seen.insert(id);
        }
        self.absorb_into_code_sets(number, &startup_ids.iter().cloned().collect());
        record_handled_ci(stored, snap);
        stored.seen_comment_ids = self.code_seen.iter().cloned().collect();
        stored.agent_comment_ids = self.code_agent_ids.iter().cloned().collect();
        self.remember(stored, SessionSlot::Code);
        stored.save(path)?;
        self.log(self.say(
            "起動時の状況確認で扱ったコメントを記録しました",
            "recorded the comments handled by the launch situation check",
        ));
        let fresh = match self.gh.pr(number) {
            Ok(fresh) => fresh,
            Err(err) => {
                self.log_kind(LogKind::Error, format!("merge re-check failed: {err:#}"));
                return Ok((false, true));
            }
        };
        self.log(merge_ready_line(&fresh.merge));
        // The survey refetch is the same moment as the poll above. Counting it
        // would merge about a second after launch when CI has not registered.
        let leave = accept_survey_refetch(
            &mut self.merge_streak,
            &fresh.head_sha,
            is_merge_ready(&fresh.merge),
        );
        Ok((leave, true))
    }

    fn run_code_fix(
        &mut self,
        number: u64,
        snap: &PrSnap,
        path: &std::path::Path,
        stored: &mut GhostState,
        ci_failed: bool,
        new_notes: Vec<Note>,
        created_here: bool,
    ) -> Result<()> {
        self.announce_fix(ci_failed, snap, &new_notes);
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
        let user_notes = self.take_queued_for_prompt();
        let push = self.arm_code_push(snap);
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
                &push,
                created_here,
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
                &push,
            )
        });
        let prompted: BTreeSet<String> = new_notes.iter().map(|n| n.id.clone()).collect();
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        if !ok {
            self.requeue_after_failed_send(user_notes);
        }
        if self.quit {
            return Ok(());
        }
        if !ok {
            self.log(self.say(
                "エージェントのターンが失敗したので、コメントも CI も処理済みにしません",
                "agent turn failed; not marking comments or CI as handled",
            ));
            return Ok(());
        }
        for id in &prompted {
            self.code_seen.insert(id.clone());
        }
        self.absorb_into_code_sets(number, &prompted);
        record_handled_ci(stored, snap);
        stored.seen_comment_ids = self.code_seen.iter().cloned().collect();
        stored.agent_comment_ids = self.code_agent_ids.iter().cloned().collect();
        self.remember(stored, SessionSlot::Code);
        stored.save(path)?;
        self.log(self.say(
            "対応したコメントだけを記録しました",
            "recorded the comments this turn handled",
        ));
        let fresh = match self.gh.pr(number) {
            Ok(fresh) => fresh,
            Err(err) => {
                self.log_kind(LogKind::Error, format!("merge re-check failed: {err:#}"));
                return Ok(());
            }
        };
        self.log(merge_ready_line(&fresh.merge));
        let _ = self.offer_if_merge_repeated(number, &fresh)?;
        Ok(())
    }

    /// Offer merge only when this head was already merge-ready on the previous look.
    fn offer_if_merge_repeated(&mut self, number: u64, snap: &PrSnap) -> Result<bool> {
        let ready = is_merge_ready(&snap.merge);
        let confirmed = self.merge_streak.observe(&snap.head_sha, ready);
        if ready && !confirmed {
            self.log_merge_wait(&snap.head_sha);
            return Ok(false);
        }
        if confirmed {
            return self.offer_merge(number, snap);
        }
        Ok(false)
    }

    fn log_merge_wait(&mut self, sha: &str) {
        let short = short_sha(sha);
        let secs = self.spec.interval.as_secs();
        self.log(self.say(
            &format!(
                "merge-ready を一度確認しました（{short}）。{secs}秒以上あとに同じ head なら merge 処理を行います。"
            ),
            &format!(
                "Saw merge-ready once ({short}). The same head at least {secs}s later will run the merge action."
            ),
        ));
    }

    fn offer_merge(&mut self, number: u64, snap: &PrSnap) -> Result<bool> {
        let notes: BTreeSet<String> = snap.notes.iter().map(|note| note.id.clone()).collect();
        let feedback = (
            snap.feedback.reviews,
            snap.feedback.inline,
            snap.feedback.comments,
        );
        if !merge_prompt_again(
            self.merge_hold.as_ref(),
            &snap.head_sha,
            &notes,
            feedback,
            self.turns,
        ) {
            return Ok(false);
        }
        let initial = MergeStyle::from_flag(self.spec.merge_method.as_deref());
        let message = merge_question(number, initial, self.spec.lang);
        let (answer, method) =
            match auto_merge_answer(self.spec.merge_yes, self.spec.merge_no, stdin_is_tty()) {
                Some(answer) => {
                    let why = if self.spec.merge_yes {
                        "y (--merge-yes)"
                    } else if self.spec.merge_no {
                        "n (--merge-no)"
                    } else if stdin_is_tty() {
                        "n"
                    } else {
                        "n (not a TTY)"
                    };
                    self.log(format!("{message} {why}"));
                    (answer, initial)
                }
                None => {
                    let _ = self.tx.send(UiEvent::Confirm {
                        number,
                        message,
                        kind: ConfirmKind::Merge(initial),
                        lang: self.spec.lang,
                    });
                    self.status.phase = "confirm".into();
                    self.push_status();
                    match self.wait_confirm()? {
                        None => return Ok(true),
                        Some((answer, chosen)) => {
                            (confirm_merge_answer(answer), chosen.unwrap_or(initial))
                        }
                    }
                }
            };
        match answer {
            MergeAnswer::Continue => {
                self.merge_hold = Some(MergeHold {
                    sha: snap.head_sha.clone(),
                    notes,
                    reviews: feedback.0,
                    inline: feedback.1,
                    comments: feedback.2,
                    turns: self.turns,
                });
                self.log(self.say(
                    "監視を続けます。head、レビュー、コメント、またはエージェントのターンが変わったら、また尋ねます。",
                    "Continuing to watch. The merge prompt returns after the head, a review or comment, or an agent turn changes.",
                ));
                Ok(false)
            }
            MergeAnswer::Exit => {
                self.stop(
                    0,
                    self.say(
                        &format!("PR #{number} はマージせずに終了します。"),
                        &format!("Exiting without merging PR #{number}."),
                    ),
                );
                Ok(true)
            }
            MergeAnswer::Merge => self.merge_now(number, &snap.head_sha, method),
        }
    }

    fn merge_now(&mut self, number: u64, expected_sha: &str, method: MergeStyle) -> Result<bool> {
        let fresh = match self.gh.pr(number) {
            Ok(snap) => snap,
            Err(err) => {
                self.log_kind(LogKind::Error, format!("merge re-check failed: {err:#}"));
                return Ok(false);
            }
        };
        if !commit_merge(is_merge_ready(&fresh.merge), expected_sha, &fresh.head_sha) {
            self.log(self.say(
                "head SHA が動いたか、もうマージ可能ではないので merge しません。監視を続けます。",
                "Head SHA moved or the PR is no longer merge-ready. Not merging; continuing to watch.",
            ));
            return Ok(false);
        }
        let flag = method.flag();
        self.log(self.say(
            &format!("merge 方法: {flag}"),
            &format!("merge method: {flag}"),
        ));
        match self.gh.merge_pr(number, Some(flag), &fresh.head_sha) {
            Ok(()) => {
                let message = if self.spec.merge_yes {
                    self.say(
                        &format!("--merge-yes により PR #{number} を merge しました。"),
                        &format!("Merged PR #{number} because of --merge-yes."),
                    )
                } else {
                    self.say(
                        &format!("PR #{number} を merge しました。"),
                        &format!("Merged PR #{number}."),
                    )
                };
                self.stop(0, message);
                Ok(true)
            }
            Err(err) => {
                let detail = format!("{err:#}");
                let transient =
                    crate::github::failure_is_transient(&err) && !merge_method_rejected(&detail);
                let flag = method.flag();
                let message = if merge_method_rejected(&detail) {
                    self.say(
                        &format!(
                            "方法 {flag} はこのリポジトリで許可されていません。自動では切り替えません。--merge-method で別の方法を指定するか、確認画面の m キーで切り替えてください。\n{detail}"
                        ),
                        &format!(
                            "Method {flag} is not allowed on this repository. ghost will not switch methods. Pass --merge-method, or press m on the confirm dialog.\n{detail}"
                        ),
                    )
                } else {
                    self.say(
                        &format!("gh pr merge に失敗しました: {detail}"),
                        &format!("gh pr merge failed: {detail}"),
                    )
                };
                if self.spec.merge_yes {
                    self.stop_with(1, message, transient);
                    return Ok(true);
                }
                self.log_kind(LogKind::Error, message);
                Ok(false)
            }
        }
    }

    fn absorb_into_code_sets(&mut self, number: u64, prompted: &BTreeSet<String>) {
        let Ok(fresh) = self.gh.pr(number) else {
            return;
        };
        let login = self.login();
        let mut seen = std::mem::take(&mut self.code_seen);
        let mut agent_ids = std::mem::take(&mut self.code_agent_ids);
        crate::watch::absorb_own_comments(
            &fresh.notes,
            prompted,
            &login,
            &mut seen,
            &mut agent_ids,
        );
        self.code_seen = seen;
        self.code_agent_ids = agent_ids;
    }

    fn arm_review_push(&mut self) {
        self.agent.set_push_plan(PushPlan {
            readonly: true,
            remote: String::new(),
            refspec: String::new(),
            reason: "review mode does not push".into(),
        });
    }

    fn arm_issue_push(&mut self, branch: &str) {
        self.agent.set_push_plan(PushPlan {
            readonly: false,
            remote: "origin".into(),
            refspec: format!("HEAD:{branch}"),
            reason: String::new(),
        });
    }

    fn arm_code_push(&mut self, snap: &PrSnap) -> String {
        let plan = workdir::plan_code_push(&workdir::PushQuery {
            base_owner: &self.owner,
            base_repo: &self.repo,
            head_owner: &snap.head_owner,
            head_name: &snap.head_repo,
            head_ref: &snap.head_branch,
            head_url: &snap.head_url,
            is_cross: snap.is_cross,
            maintainer_can_modify: snap.maintainer_can_modify,
        });
        let (readonly, remote, refspec, reason, head_ref) = match &plan {
            workdir::CodePush::ReadOnly { head_ref } => (
                true,
                String::new(),
                String::new(),
                format!("fork head {head_ref} is not pushable"),
                head_ref.clone(),
            ),
            workdir::CodePush::Push { remote, refspec } => (
                false,
                remote.clone(),
                refspec.clone(),
                String::new(),
                snap.head_branch.clone(),
            ),
        };
        self.agent.set_push_plan(PushPlan {
            readonly,
            remote: remote.clone(),
            refspec: refspec.clone(),
            reason,
        });
        prompt::describe_push(readonly, &remote, &refspec, &head_ref, self.spec.lang)
    }

    fn log_situation(&mut self) {
        self.log(self.say(
            "起動時の状況確認を始めます。保存された状態では判断しません。",
            "Starting the launch situation check. Saved state does not decide this turn.",
        ));
    }

    /// Notes folded into the prompt that is about to be sent.
    fn take_queued_for_prompt(&mut self) -> Vec<String> {
        let notes = std::mem::take(&mut self.queued);
        if !notes.is_empty() {
            self.log(input_delivery_log(InputPlan::SendNow, self.spec.lang));
        }
        notes
    }

    fn requeue_after_failed_send(&mut self, notes: Vec<String>) {
        if notes.is_empty() || self.quit {
            return;
        }
        let mut restored = notes;
        restored.append(&mut self.queued);
        self.queued = restored;
        self.log(input_delivery_log(InputPlan::Queue, self.spec.lang));
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
        // The launch situation check starts a new session. Later turns in this
        // process, and `i`, resume that id. A saved id is not resumed.
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
        self.log_situation();
        let user_notes = self.take_queued_for_prompt();
        let mut task = if let Some(pr) = existing {
            prompt::issue_situation(
                &self.slug,
                number,
                &issue.title,
                &issue.body,
                &branch,
                &base,
                &self.workdir,
                pr,
                &user_notes,
                self.spec.lang,
            )
        } else {
            prompt::issue_initial(
                &self.slug,
                number,
                &issue.title,
                &issue.body,
                &branch,
                &base,
                &self.workdir,
                &user_notes,
                self.spec.lang,
            )
        };
        if self.agent.session_id().is_some() {
            task = format!(
                "{}{task}",
                prompt::issue_resume_prefix(self.spec.lang, number, &branch)
            );
        }
        let task = self.decorate(task);
        self.arm_issue_push(&branch);
        let ok = self.dispatch_task(&task)?;
        self.note_setup_result(ok);
        if !ok {
            self.requeue_after_failed_send(user_notes);
        }
        if self.quit {
            return Ok(());
        }
        self.remember(&mut istate, SessionSlot::Code);
        istate.save(&ipath)?;
        if let Some(pr) = existing {
            self.baseline_new_pr(pr, false)?;
            if self.quit {
                return Ok(());
            }
            return self.continue_as_code(pr, false);
        }
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
        self.continue_as_code(pr, true)
    }

    /// Seen-set stays empty. Only viewer comments that predate the PR are agent ids.
    fn baseline_new_pr(&mut self, pr: u64, created_here: bool) -> Result<()> {
        let ppath = state::pr_state_path(
            &self.state_root,
            &self.owner,
            &self.repo,
            pr,
            SessionSlot::Code,
        );
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

    fn continue_as_code(&mut self, pr: u64, created_here: bool) -> Result<()> {
        if !self.hold_pr_slot(pr, SessionSlot::Code) {
            return Ok(());
        }
        self.log(format!(
            "continuing as --code on PR #{pr} in the same {} session",
            self.agent.kind()
        ));
        // Same AgentSession. code_loop does not resume a saved id.
        self.code_loop(pr, CodeLaunch::Handoff { created_here })
    }

    fn hold_pr_slot(&mut self, number: u64, slot: SessionSlot) -> bool {
        let name = state::pr_slot_name(number, slot);
        if self.use_slot(&name) {
            return true;
        }
        self.stop(
            1,
            self.say(
                "worktree をロックできなかったので、メインクローンでは作業しません。",
                "Could not lock the worktree, so ghost will not work in the main clone.",
            ),
        );
        false
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
                Ok(UiCmd::Choose(Side::Review)) => {
                    if !self.hold_pr_slot(number, SessionSlot::Review) {
                        return Ok(());
                    }
                    return self.review_loop(number);
                }
                Ok(UiCmd::Choose(Side::Code)) => {
                    if !self.hold_pr_slot(number, SessionSlot::Code) {
                        return Ok(());
                    }
                    return self.code_loop(number, CodeLaunch::Survey);
                }
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(other) => self.handle_cmd(other)?,
            }
        }
    }

    fn picker_flow(&mut self, issues_only: bool) -> Result<()> {
        self.status.mode = if issues_only { "issues" } else { "picker" }.into();
        self.status.target = self.spec.target.display_url();
        self.status.phase = "loading".into();
        self.push_status();
        let items = if issues_only {
            self.log("loading open issues");
            self.gh.open_issues()?
        } else {
            self.log("loading open pull requests and issues");
            self.gh.open_items()?
        };
        let _ = self.tx.send(if issues_only {
            UiEvent::IssuePicker(items)
        } else {
            UiEvent::Picker(items)
        });
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
        let slot = match (kind, side) {
            (crate::github::ItemKind::Pull, Some(Side::Review)) => {
                Some(state::pr_slot_name(number, SessionSlot::Review))
            }
            (crate::github::ItemKind::Pull, Some(Side::Code)) => {
                Some(state::pr_slot_name(number, SessionSlot::Code))
            }
            (crate::github::ItemKind::Issue, _) => Some(format!("issue-{number}")),
            (crate::github::ItemKind::Pull, None) => None,
        };
        if let Some(slot) = slot {
            if !self.use_slot(&slot) {
                self.stop(
                    1,
                    self.say(
                        "worktree をロックできなかったので、メインクローンでは作業しません。",
                        "Could not lock the worktree, so ghost will not work in the main clone.",
                    ),
                );
                return Ok(());
            }
        }
        match (kind, side) {
            (crate::github::ItemKind::Pull, Some(Side::Review)) => self.review_loop(number),
            (crate::github::ItemKind::Pull, Some(Side::Code)) => {
                self.code_loop(number, CodeLaunch::Survey)
            }
            (crate::github::ItemKind::Issue, _) => self.issue_flow(number),
            (crate::github::ItemKind::Pull, None) => self.ask_mode(number),
        }
    }

    /// Open pull requests only. The mode was chosen with `--review` or `--code`.
    fn pulls_flow(&mut self, listed: ListedPull) -> Result<()> {
        let side = match listed {
            ListedPull::Review => Side::Review,
            ListedPull::Code => Side::Code,
        };
        self.status.mode = "pulls".into();
        self.status.target = self.spec.target.display_url();
        self.status.phase = "loading".into();
        self.push_status();
        self.log("loading open pull requests");
        let items = self.gh.open_prs()?;
        let _ = self.tx.send(UiEvent::PullPicker { items, side });
        self.status.phase = "choose".into();
        self.push_status();
        let number = loop {
            match self.rx.recv() {
                Ok(UiCmd::Pick {
                    kind: crate::github::ItemKind::Pull,
                    number,
                    ..
                }) => break number,
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(());
                }
                Ok(other) => self.handle_cmd(other)?,
            }
        };
        let slot = match side {
            Side::Review => state::pr_slot_name(number, SessionSlot::Review),
            Side::Code => state::pr_slot_name(number, SessionSlot::Code),
        };
        if !self.use_slot(&slot) {
            self.stop(
                1,
                self.say(
                    "worktree をロックできなかったので、メインクローンでは作業しません。",
                    "Could not lock the worktree, so ghost will not work in the main clone.",
                ),
            );
            return Ok(());
        }
        match side {
            Side::Review => self.review_loop(number),
            Side::Code => self.code_loop(number, CodeLaunch::Survey),
        }
    }

    /// Refetch open issues on each check. One issue runs at a time.
    /// An empty list, or a failed `gh` call, waits for the next interval instead of exiting.
    fn auto_loop(&mut self) -> Result<()> {
        self.status.mode = "auto".into();
        self.status.target = self.spec.target.display_url();
        self.push_status();
        let path = state::auto_state_path(&self.state_root, &self.owner, &self.repo);
        loop {
            if self.quit {
                self.stop(0, "quit".into());
                return Ok(());
            }
            self.status.phase = "checking".into();
            self.push_status();
            let fetched = self.fetch_auto(&path);
            let (issues, prs, mut stored) = match fetched {
                AutoFetch::Wait(message) => {
                    self.note_auto_wait(message);
                    if self.wait_interval()? {
                        return Ok(());
                    }
                    continue;
                }
                AutoFetch::Ready {
                    issues,
                    prs,
                    stored,
                    warning,
                } => {
                    if let Some(warning) = warning {
                        self.log_kind(LogKind::Error, warning);
                    }
                    (issues, prs, stored)
                }
            };
            let created_path =
                state::created_issues_path(&self.state_root, &self.owner, &self.repo);
            let created_prs_path =
                state::created_prs_path(&self.state_root, &self.owner, &self.repo);
            let mut busy = Vec::new();
            loop {
                let now = crate::auto::now_rfc3339();
                let created = state::read_created_issues(&created_path);
                let created_prs = state::read_created_prs(&created_prs_path);
                let choice = crate::auto::poll(&crate::auto::PollIn {
                    issues: &issues,
                    failed: &stored.failed,
                    prs: &prs,
                    trust: self.spec.trust_issues,
                    created: &created,
                    created_prs: &created_prs,
                    now: &now,
                    busy: &busy,
                });
                let mut changed = false;
                for skip in &choice.skipped {
                    if Self::remember_skip(&mut stored, skip) {
                        changed = true;
                    }
                    self.log(self.skip_line(skip));
                }
                if changed {
                    if let Err(err) = stored.save(&path) {
                        self.log_kind(LogKind::Error, format!("auto state: {err:#}"));
                    }
                }
                match choice.action {
                    crate::auto::AutoAction::Idle => {
                        self.log(self.say(
                            "未処理のオープンな issue はありません。次の確認まで待ちます。",
                            "No open issue is waiting. Checking again after the interval.",
                        ));
                        if self.wait_interval()? {
                            return Ok(());
                        }
                        break;
                    }
                    crate::auto::AutoAction::Run(number) => {
                        let head = issues
                            .iter()
                            .find(|row| row.number == number)
                            .map(|row| crate::auto::current_head(row, &prs))
                            .unwrap_or_default();
                        self.log(self.say(
                            &format!("issue #{number} を始めます。"),
                            &format!("Taking issue #{number}."),
                        ));
                        if !self.begin_auto_issue(number, &path, &head)? {
                            busy.push(number);
                            continue;
                        }
                        break;
                    }
                }
            }
        }
    }

    /// A failed list is a wait, never a fatal error.
    fn fetch_auto(&self, path: &std::path::Path) -> AutoFetch {
        let issues = match self.gh.auto_issues() {
            Ok(rows) => rows,
            Err(err) => return AutoFetch::Wait(format!("gh issue list: {err:#}")),
        };
        let prs = match self.gh.auto_pr_links() {
            Ok(rows) => rows,
            Err(err) => return AutoFetch::Wait(format!("gh pr list: {err:#}")),
        };
        match state::AutoState::load(path) {
            Ok(loaded) => AutoFetch::Ready {
                issues,
                prs,
                stored: loaded.state,
                warning: loaded.warning,
            },
            Err(err) => AutoFetch::Wait(format!("auto state: {err:#}")),
        }
    }

    fn note_auto_wait(&mut self, message: String) {
        self.log_kind(LogKind::Error, message);
    }

    /// `false` when another ghost holds `issue-N`. That issue is skipped this round.
    fn begin_auto_issue(
        &mut self,
        number: u64,
        path: &std::path::Path,
        head: &str,
    ) -> Result<bool> {
        let slot = format!("issue-{number}");
        if !self.use_slot_inner(&slot, false) {
            return Ok(false);
        }
        self.auto_issue = Some(number);
        self.auto_release = None;
        let result = self.issue_flow(number);
        let release = self.auto_release.take();
        let user_quit = self.quit && release.is_none();
        self.auto_issue = None;
        if user_quit {
            return Ok(true);
        }
        if let Err(err) = result {
            let transient = crate::github::failure_is_transient(&err);
            let reason = format!("{err:#}");
            self.log_kind(LogKind::Error, reason.clone());
            if let Err(save_err) = self.record_auto_failure(number, &reason, path, head, transient)
            {
                self.log_kind(LogKind::Error, format!("auto state: {save_err:#}"));
            }
        } else if let Some(release) = release {
            if release.code != 0 {
                if let Err(save_err) = self.record_auto_failure(
                    number,
                    &release.message,
                    path,
                    head,
                    release.transient,
                ) {
                    self.log_kind(LogKind::Error, format!("auto state: {save_err:#}"));
                }
            }
        }
        self.prepare_next_issue();
        Ok(true)
    }

    fn remember_skip(stored: &mut state::AutoState, skip: &crate::auto::Skip) -> bool {
        match skip {
            crate::auto::Skip::Closed { number, pr } if !stored.has_permanent(*number) => {
                stored.record(
                    *number,
                    &format!("pull request #{pr} was closed; not redoing this issue"),
                    false,
                    "",
                    "",
                );
                true
            }
            _ => false,
        }
    }

    fn skip_line(&self, skip: &crate::auto::Skip) -> String {
        match skip {
            crate::auto::Skip::Failed { number, reason } => self.say(
                &format!("issue #{number} は以前失敗したので飛ばします: {reason}"),
                &format!("skipping issue #{number}; it failed earlier: {reason}"),
            ),
            crate::auto::Skip::OpenPr { number, pr } => self.say(
                &format!("issue #{number} は開いている PR #{pr} があるので飛ばします。"),
                &format!("skipping issue #{number}; open PR #{pr} already covers it."),
            ),
            crate::auto::Skip::Closed { number, pr } => self.say(
                &format!("issue #{number} は PR #{pr} が閉じられているのでやり直しません。"),
                &format!("skipping issue #{number}; pull request #{pr} is closed."),
            ),
            crate::auto::Skip::Untrusted {
                number,
                author,
                association,
            } => self.say(
                &format!(
                    "issue #{number} の投稿者 {author}（{association}）は --trust-issues の対象外なので飛ばします。"
                ),
                &format!(
                    "skipping issue #{number}; {author} ({association}) is outside --trust-issues."
                ),
            ),
            crate::auto::Skip::AgentCreated { number } => self.say(
                &format!(
                    "issue #{number} はエージェントが gh issue create で開いたので飛ばします。"
                ),
                &format!("skipping issue #{number}; the agent created it with gh issue create."),
            ),
            crate::auto::Skip::Backoff { number, retry_at } => self.say(
                &format!(
                    "issue #{number} は一時的な失敗のあとなので、{retry_at} まで待つか先端が変わるまで飛ばします。"
                ),
                &format!(
                    "skipping issue #{number} until {retry_at}, or until its pull request head changes."
                ),
            ),
            crate::auto::Skip::Busy { number } => self.say(
                &format!("issue #{number} は別の ghost が作業中なので、この回は飛ばします。"),
                &format!("another ghost is using issue #{number}; skipping it this round."),
            ),
        }
    }

    fn record_auto_failure(
        &mut self,
        number: u64,
        reason: &str,
        path: &std::path::Path,
        head: &str,
        transient: bool,
    ) -> Result<()> {
        let loaded = state::AutoState::load(path)?;
        if let Some(warning) = loaded.warning {
            self.log_kind(LogKind::Error, warning);
        }
        let mut stored = loaded.state;
        let attempts = stored.attempts(number).saturating_add(1);
        let transient = crate::auto::keep_transient(transient, attempts);
        let retry_at = if transient {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let wait = agent_backoff(attempts).as_secs();
            crate::auto::format_rfc3339(now.saturating_add(wait))
        } else {
            String::new()
        };
        let head = if transient { head } else { "" };
        stored.record(number, reason, transient, &retry_at, head);
        stored.save(path)?;
        if transient {
            self.log(self.say(
                &format!(
                    "issue #{number} は一時的な失敗です（{attempts}/{}）。待つか、プルリクエストの先端が変わったらやり直します。",
                    crate::auto::MAX_TRANSIENT_ATTEMPTS
                ),
                &format!(
                    "issue #{number} failed temporarily ({attempts}/{}). Retrying after a wait, or when the pull request head changes.",
                    crate::auto::MAX_TRANSIENT_ATTEMPTS
                ),
            ));
        } else if attempts >= crate::auto::MAX_TRANSIENT_ATTEMPTS {
            self.log(self.say(
                &format!(
                    "issue #{number} は {attempts} 回失敗したので、やり直さず記録します。"
                ),
                &format!(
                    "issue #{number} failed {attempts} times, so it stays recorded and is not retried."
                ),
            ));
        } else {
            self.log(self.say(
                &format!("issue #{number} を失敗として記録しました。"),
                &format!("Recorded issue #{number} as failed."),
            ));
        }
        Ok(())
    }

    fn prepare_next_issue(&mut self) {
        self.quit = false;
        self.turns = 0;
        self.fail_streak = 0;
        self.queued.clear();
        self.delivering_queue = false;
        self.turn_signal.clear();
        self.interrupted = false;
        self.last_idle_sha = None;
        self.merge_streak = MergeReadyStreak::with_gap(self.spec.interval);
        self.merge_hold = None;
        self.code_pr = None;
        self.code_path = None;
        self.code_seen.clear();
        self.code_agent_ids.clear();
        self.slot_lock = None;
        self.agent.forget_session();
        self.status.turns = format!("0/{}", self.spec.max_turns);
        self.status.session = "—".into();
        self.push_status();
    }

    fn handing_off(&self) -> bool {
        self.spec.mode == RunMode::Auto && self.auto_issue.is_some()
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
        if self.slot_lock.is_none() || self.slot_dir == self.main_clone {
            self.push_setup_failure(plain_failure(
                format!("{purpose} のブランチを作業用 worktree に用意するため"),
                format!("flock {}", self.slot_dir.display()),
                format!(
                    "{purpose} を {} で作業できる状態にしてから作業して",
                    self.slot_dir.display()
                ),
                &anyhow!(
                    "the worktree lock is not held, so ghost will not check the branch out in the main clone"
                ),
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
        let fell_back = outcome.workdir == self.main_clone && self.slot_dir != self.main_clone;
        let workdir = if fell_back {
            self.slot_dir.clone()
        } else {
            outcome.workdir.clone()
        };
        if fell_back {
            self.push_setup_failure(plain_failure(
                "作業ディレクトリを決めるため",
                "best_workdir",
                format!(
                    "{} で作業できる状態にしてから作業して",
                    self.slot_dir.display()
                ),
                &anyhow!("refusing to fall back to the main clone"),
            ));
        }
        self.workdir = workdir.clone();
        self.agent.set_workdir(workdir);
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

    /// A missing worktree must not make the agent spawn fail with ENOENT.
    /// The process starts in `~/.ghost/work`. The prompt already has the failed step.
    fn ensure_spawn_dir(&mut self) {
        if self.agent.workdir_is_dir() {
            return;
        }
        let fallback = self.agent.fallback_dir();
        if self.cwd_fallback_logged {
            return;
        }
        self.cwd_fallback_logged = true;
        self.log(self.say(
            &format!(
                "作業ディレクトリ {} が無いので、エージェントは {} で起動します。失敗した準備手順はプロンプトに渡します。",
                self.workdir.display(),
                fallback.display()
            ),
            &format!(
                "Working directory {} does not exist. Starting the agent in {} and handing it the failed setup step.",
                self.workdir.display(),
                fallback.display()
            ),
        ));
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
            self.ensure_spawn_dir();
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
            let exit_code = self.agent.last_exit_code();
            let output = self.agent.recent_output().to_string();
            let counts = counts_as_agent_turn(&launch, exit_code, &output);
            let missing = resume_unavailable(exit_code, &output);
            if !counts {
                self.turns = self.turns.saturating_sub(1);
                self.status.turns = format!("{}/{}", self.turns, self.spec.max_turns);
                if missing {
                    self.log(self.say(
                        "resume できないセッション（rollout が無い、または不明）なので、この試行はターンに数えません",
                        "resume target is missing, so this attempt does not count as a turn",
                    ));
                } else {
                    self.log(self.say(
                        "resume の起動引数が拒否されたため、この試行はターンに数えません",
                        "resume rejected its arguments, so this attempt does not count as a turn",
                    ));
                }
            } else {
                self.log(format!(
                    "agent turn finished ({}/{})",
                    self.turns, self.spec.max_turns
                ));
            }
            self.refresh_session_status();
            self.push_status();
            let broken_pipe = self.agent.prompt_was_refused();
            if broken_pipe {
                self.log(self.say(
                    "エージェントがプロンプトを読まずに終了しました（EPIPE）。このターンは失敗です。",
                    "The agent closed the prompt pipe (EPIPE). This turn failed.",
                ));
            }
            let ok = !broken_pipe
                && record_progress(
                    self.interrupted,
                    exit_code,
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
            if allow_fresh && matches!(launch, Launch::Resume { .. }) {
                self.log(self.say(
                    "resume に失敗したので、同じ文脈で新しいセッションを開始します",
                    "resume failed; starting a fresh session with the same context",
                ));
                self.agent.forget_session();
                allow_fresh = false;
                // A missing session or a rejected argv is not a failed turn:
                // start the fresh session now, with no backoff and no streak.
                if counts {
                    self.backoff_after_failure()?;
                    if self.quit {
                        return Ok(false);
                    }
                }
                continue;
            }
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".into());
            self.log(self.say(
                &format!("エージェントのターンが失敗しました（exit {code}）"),
                &format!("agent turn failed (exit {code})"),
            ));
            if matches!(launch, Launch::Start { .. }) && !output.trim().is_empty() {
                self.log(self.say(
                    &format!(
                        "新しいセッションの出力:\n{}",
                        truncate(output.trim(), 2_000)
                    ),
                    &format!("fresh session output:\n{}", truncate(output.trim(), 2_000)),
                ));
            }
            if counts {
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
                if !self.delivering_queue {
                    self.turn_signal = self.agent.recent_output().to_string();
                    if !self.quit {
                        self.delivering_queue = true;
                        let flushed = self.flush_queued();
                        self.delivering_queue = false;
                        flushed?;
                    }
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
        if !self.queued.is_empty() && !self.delivering_queue && !self.agent.turn_open() {
            self.delivering_queue = true;
            let flushed = self.flush_queued();
            self.delivering_queue = false;
            flushed?;
            if self.quit {
                return Ok(true);
            }
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

    fn wait_confirm(&mut self) -> Result<Option<(ConfirmAnswer, Option<MergeStyle>)>> {
        loop {
            match self.rx.recv() {
                Ok(UiCmd::Confirm { answer, method }) => return Ok(Some((answer, method))),
                Ok(UiCmd::Quit) | Err(_) => {
                    self.stop(0, "quit".into());
                    return Ok(None);
                }
                Ok(UiCmd::Input(text)) => self.on_input(text, self.agent.turn_open())?,
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
            UiCmd::Pick { .. } | UiCmd::Choose(_) | UiCmd::Confirm { .. } => {}
        }
        Ok(())
    }

    fn on_input(&mut self, text: String, during_turn: bool) -> Result<()> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.log_kind(LogKind::User, text.clone());
        match plan_user_input(during_turn, self.agent.session_id().is_some()) {
            InputPlan::Queue => {
                self.queued.push(text);
                self.log(input_delivery_log(InputPlan::Queue, self.spec.lang));
                Ok(())
            }
            InputPlan::SendNow => {
                self.queued.push(text);
                self.flush_queued()
            }
        }
    }

    /// Send queued lines into the current session, or start one when none is alive.
    /// A failed send puts the lines back and logs that they are still queued.
    fn flush_queued(&mut self) -> Result<()> {
        if self.queued.is_empty() || self.quit {
            return Ok(());
        }
        let notes = std::mem::take(&mut self.queued);
        self.log(input_delivery_log(InputPlan::SendNow, self.spec.lang));
        let prompt = self.decorate(prompt::user_nudge(
            self.spec.lang,
            &notes.join("\n\n"),
            true,
        ));
        let before = self.comment_ids_now();
        let ok = self.dispatch_task(&prompt)?;
        self.note_setup_result(ok);
        if ok {
            self.absorb_queued_replies(&before);
        } else {
            self.requeue_after_failed_send(notes);
        }
        Ok(())
    }

    fn comment_ids_now(&mut self) -> BTreeSet<String> {
        let Some(number) = self.code_pr else {
            return BTreeSet::new();
        };
        match self.gh.pr(number) {
            Ok(snap) => snap.notes.into_iter().map(|note| note.id).collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    /// A queued follow-up can post comments. Those ids are the agent's, so the
    /// next code poll must not reply to them.
    fn absorb_queued_replies(&mut self, before: &BTreeSet<String>) {
        let Some(number) = self.code_pr else {
            return;
        };
        let Some(path) = self.code_path.clone() else {
            return;
        };
        self.absorb_into_code_sets(number, before);
        let Ok(mut stored) = GhostState::load(&path) else {
            return;
        };
        stored.seen_comment_ids = self.code_seen.iter().cloned().collect();
        stored.agent_comment_ids = self.code_agent_ids.iter().cloned().collect();
        if stored.save(&path).is_ok() {
            self.log(self.say(
                "送信したフォローのあいだに付いた自分のコメントは、新しい作業にしません",
                "comments posted during the queued follow-up are not new work",
            ));
        }
    }

    fn find_pr_after(&mut self, issue: u64, branch: &str) -> Result<Option<u64>> {
        for attempt in 0..3 {
            match self.gh.find_issue_pr(issue, branch) {
                Ok(Some(number)) => return Ok(Some(number)),
                Ok(None) => {}
                Err(err) if self.handing_off() => {
                    let transient = crate::github::failure_is_transient(&err);
                    self.stop_with(
                        1,
                        format!("could not look up a pull request for issue #{issue}: {err:#}"),
                        transient,
                    );
                    return Ok(None);
                }
                Err(err) => return Err(err),
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
        let reason = format!(
            "the agent finished but no pull request for issue #{issue} was found. Expected branch {branch} or a body containing Closes #{issue}."
        );
        if self.handing_off() {
            self.stop(1, reason);
            return Ok(None);
        }
        Err(anyhow!(reason))
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
        self.stop_with(code, message, false);
    }

    fn stop_with(&mut self, code: i32, message: String, transient: bool) {
        if self.handing_off() && message != "quit" {
            if !message.is_empty() {
                self.log(message.clone());
            }
            self.auto_release = Some(AutoRelease {
                code,
                message,
                transient,
            });
            self.agent.stop();
            self.quit = true;
            return;
        }
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

    fn use_slot(&mut self, slot: &str) -> bool {
        self.use_slot_inner(slot, true)
    }

    /// `announce` reports a busy lock as a setup failure. `--auto` passes false and skips the issue.
    fn use_slot_inner(&mut self, slot: &str, announce: bool) -> bool {
        if self.slot_lock.is_some()
            && self.slot_dir.file_name().and_then(|name| name.to_str()) == Some(slot)
        {
            return true;
        }
        let Some(ghost_home) = self
            .main_clone
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
        else {
            if announce {
                self.push_setup_failure(plain_failure(
                    format!("{slot} の作業ディレクトリを決めるため"),
                    "resolve ghost home",
                    format!("{slot} の worktree パスが分かっている状態にしてから作業して"),
                    &anyhow!("work directory has no home"),
                ));
            }
            return false;
        };
        let dest = workdir::slot_path(&ghost_home, &self.owner, &self.repo, slot);
        match workdir::lock_slot(&dest) {
            Ok(lock) => {
                self.slot_lock = Some(lock);
                self.slot_dir = dest.clone();
                self.workdir = dest.clone();
                self.agent.set_workdir(dest);
                self.agent.set_prompt_slot(slot);
                true
            }
            Err(err) => {
                if announce {
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
                false
            }
        }
    }

    /// Filter line for this poll, including `filtered 0` when nothing was dropped.
    fn prepare_notes(&self, snap: &mut PrSnap, agent_ids: &BTreeSet<String>) -> String {
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
        RunMode::Picker | RunMode::IssueList | RunMode::PullList(_) | RunMode::Auto => {
            format!("選択された PR #{number}")
        }
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
        RunMode::IssueList => "issues",
        RunMode::PullList(_) => "pulls",
        RunMode::Auto => "auto",
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

struct AutoRelease {
    code: i32,
    message: String,
    /// Set at the `gh` failure. A reason that merely mentions a branch or an issue number stays false.
    transient: bool,
}

enum AutoFetch {
    Wait(String),
    Ready {
        issues: Vec<crate::auto::IssueRow>,
        prs: Vec<crate::auto::PrLink>,
        stored: state::AutoState,
        warning: Option<String>,
    },
}

fn short_id(id: &str) -> &str {
    let n = id.len().min(8);
    &id[..n]
}

/// What to do with a line the user submitted with `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPlan {
    /// No turn is open. Start a session when none exists, or resume, and send now.
    SendNow,
    /// A turn is running. Hold the line and send it when that turn finishes.
    Queue,
}

/// `has_session` is false when no session was started or the previous one died.
/// Those lines are still sent now: ghost starts or resumes a session for them.
pub fn plan_user_input(turn_open: bool, has_session: bool) -> InputPlan {
    if turn_open {
        InputPlan::Queue
    } else {
        let _ = has_session;
        InputPlan::SendNow
    }
}

pub fn input_delivery_log(plan: InputPlan, lang: Lang) -> String {
    match (plan, lang) {
        (InputPlan::SendNow, Lang::Ja) => "→ エージェントに送信".into(),
        (InputPlan::SendNow, Lang::En) => "→ sent to the agent".into(),
        (InputPlan::Queue, Lang::Ja) => "送信待ち。ターン終了後に → エージェントに送信".into(),
        (InputPlan::Queue, Lang::En) => "queued; → sent to the agent when the turn finishes".into(),
    }
}

fn stdin_is_tty() -> bool {
    // Do not open /dev/tty to ask. crossterm does that when stdin is not a
    // terminal, and with no controlling terminal the open fails with ENXIO
    // (os error 6) before any confirm can take the non-TTY answer.
    crate::tui::should_open_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )
}

pub fn confirm_merge_answer(answer: ConfirmAnswer) -> MergeAnswer {
    match answer {
        ConfirmAnswer::Yes => MergeAnswer::Merge,
        ConfirmAnswer::Continue => MergeAnswer::Continue,
        ConfirmAnswer::No => MergeAnswer::Exit,
    }
}

fn record_handled_ci(stored: &mut GhostState, snap: &PrSnap) {
    if snap.merge.checks == crate::merge::ChecksVerdict::Failed {
        stored.handled_ci_sha = Some(snap.head_sha.clone());
        stored.handled_failing_checks = snap.failing_checks.clone();
    } else {
        stored.handled_ci_sha = None;
        stored.handled_failing_checks.clear();
    }
}

fn ci_handled(stored: &GhostState) -> Option<CiHandled> {
    let sha = stored.handled_ci_sha.clone()?;
    Some(CiHandled::new(sha, stored.handled_failing_checks.clone()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        agent_backoff, confirm_merge_answer, empty_repo_notice, input_delivery_log, panic_message,
        plan_user_input, stop_after_failures, InputPlan, MAX_CONSECUTIVE_AGENT_FAILURES,
    };
    use crate::lang::Lang;
    use crate::merge::MergeAnswer;
    use crate::tui::ConfirmAnswer;

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

    #[test]
    fn user_input_is_delivered_when_idle_during_a_turn_and_with_no_session() {
        assert_eq!(plan_user_input(false, true), InputPlan::SendNow);
        assert_eq!(plan_user_input(true, true), InputPlan::Queue);
        assert_eq!(plan_user_input(false, false), InputPlan::SendNow);
        assert_eq!(plan_user_input(true, false), InputPlan::Queue);

        let sent = input_delivery_log(InputPlan::SendNow, Lang::Ja);
        let queued = input_delivery_log(InputPlan::Queue, Lang::Ja);
        assert_eq!(sent, "→ エージェントに送信");
        assert!(queued.contains("送信待ち"));
        assert!(queued.contains("→ エージェントに送信"));
        assert!(input_delivery_log(InputPlan::Queue, Lang::En).contains("queued"));
    }

    #[test]
    fn merge_answers_stay_separate_from_other_flows() {
        assert_eq!(confirm_merge_answer(ConfirmAnswer::Yes), MergeAnswer::Merge);
        assert_eq!(confirm_merge_answer(ConfirmAnswer::No), MergeAnswer::Exit);
        assert_eq!(
            confirm_merge_answer(ConfirmAnswer::Continue),
            MergeAnswer::Continue
        );
    }

    #[test]
    fn worker_panic_is_reported_and_does_not_stay_running() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&payload), "worker panicked: boom");
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&payload), "worker panicked: boom");
    }

    fn path_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    struct RestoreVar(&'static str, Option<String>);
    impl Drop for RestoreVar {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    fn merge_ctx(
        merge_yes: bool,
    ) -> (
        super::Ctx,
        std::sync::mpsc::Receiver<crate::tui::UiEvent>,
        std::sync::mpsc::Sender<crate::tui::UiCmd>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let home = std::env::temp_dir();
        let target = crate::target::parse_target("github.com/acme/app/pull/1").unwrap();
        let ctx = super::Ctx {
            spec: super::RunSpec {
                header: String::new(),
                target,
                mode: super::RunMode::Code,
                interval: Duration::from_secs(20),
                agent: None,
                merge_yes,
                merge_no: false,
                merge_method: None,
                max_turns: 20,
                lang: Lang::Ja,
                ssh_key: None,
                trust: crate::watch::CommentTrust::All,
                trust_issues: crate::watch::CommentTrust::Collaborators,
            },
            tx,
            rx: cmd_rx,
            gh: crate::github::Gh::new("acme/app"),
            slug: "acme/app".into(),
            owner: "acme".into(),
            repo: "app".into(),
            workdir: home.clone(),
            slot_dir: home.clone(),
            main_clone: home.clone(),
            setup_failures: Vec::new(),
            setup_handed: false,
            slot_lock: None,
            login: None,
            agent: crate::agent::AgentSession::new(
                crate::agent::AgentKind::Codex,
                home.clone(),
                &home,
                "pr-1-code",
            ),
            turns: 0,
            fail_streak: 0,
            queued: Vec::new(),
            delivering_queue: false,
            turn_signal: String::new(),
            status: crate::tui::Status::default(),
            quit: false,
            interrupted: false,
            last_idle_sha: None,
            merge_streak: crate::merge::MergeReadyStreak::with_gap(Duration::from_secs(20)),
            merge_hold: None,
            cwd_fallback_logged: false,
            code_pr: None,
            code_path: None,
            code_seen: std::collections::BTreeSet::new(),
            code_agent_ids: std::collections::BTreeSet::new(),
            state_root: home,
            auto_issue: None,
            auto_release: None,
        };
        (ctx, rx, cmd_tx)
    }

    #[test]
    fn merge_no_exits_successfully_without_a_confirmation() {
        let (mut ctx, rx, _cmd) = merge_ctx(false);
        ctx.spec.merge_no = true;
        let snap = crate::watch::PrSnap {
            number: 1,
            title: "title".into(),
            life: crate::merge::PrLife::Open,
            draft: false,
            head_sha: "abc".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
            merge: crate::merge::MergeInput {
                life: crate::merge::PrLife::Open,
                draft: false,
                mergeable: crate::merge::Mergeability::Mergeable,
                merge_state: crate::merge::MergeState::Clean,
                checks: crate::merge::ChecksVerdict::AllGreen,
                approvals: 1,
                changes_requested: false,
            },
            failing_checks: Vec::new(),
            notes: Vec::new(),
            ci_label: String::new(),
            author: "me".into(),
            created_at: String::new(),
            feedback: crate::watch::FeedbackCounts::default(),
            feedback_warnings: Vec::new(),
            comments_unavailable: false,
            is_cross: false,
            head_owner: "acme".into(),
            head_repo: "app".into(),
            head_url: String::new(),
            maintainer_can_modify: true,
        };

        assert!(ctx.offer_merge(1, &snap).unwrap());
        assert!(ctx.quit);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events
            .iter()
            .all(|event| !matches!(event, crate::tui::UiEvent::Confirm { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::tui::UiEvent::Exit { code: 0, message }
                if message == "PR #1 はマージせずに終了します。"
        )));
    }

    fn write_exe(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn auto_moves_on_without_leaving_the_process() {
        let (mut ctx, rx, _cmd) = merge_ctx(false);
        ctx.spec.mode = super::RunMode::Auto;
        ctx.auto_issue = Some(4);
        ctx.stop(0, "Exiting without merging PR #9.".into());
        assert!(ctx.quit);
        assert_eq!(ctx.auto_release.as_ref().map(|row| row.code), Some(0));
        let mut saw_exit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, crate::tui::UiEvent::Exit { .. }) {
                saw_exit = true;
            }
        }
        assert!(!saw_exit, "a finished issue returns to --auto");
        ctx.auto_issue = None;
        ctx.prepare_next_issue();
        assert!(!ctx.quit);
        ctx.stop(0, "quit".into());
        saw_exit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, crate::tui::UiEvent::Exit { .. }) {
                saw_exit = true;
            }
        }
        assert!(saw_exit, "q still leaves ghost");
    }

    /// `merge_now` re-reads the head. A moved SHA must not call `gh pr merge`.
    #[test]
    fn merge_now_rechecks_head_sha_before_gh_merge() {
        let _lock = path_lock();
        let dir = std::env::temp_dir().join(format!(
            "ghost-merge-now-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("gh");
        let view = dir.join("view.json");
        let marker = dir.join("merged");
        write_exe(
            &stub,
            "#!/bin/sh\ncase \" $* \" in\n  *\" pr merge \"*)\n    if [ \"${GHOST_TEST_MERGE_CODE:-0}\" != 0 ]; then echo \"$GHOST_TEST_MERGE_ERR\" >&2; exit 1; fi\n    : > \"$GHOST_TEST_MARKER\"\n    exit 0 ;;\n  *\" pr view \"*) cat \"$GHOST_TEST_VIEW\"; exit 0 ;;\nesac\nprintf '%s\\n' '[]'\nexit 0\n",
        );
        let old_path = std::env::var("PATH").ok();
        let _path = RestoreVar("PATH", old_path.clone());
        let mut path = dir.display().to_string();
        if let Some(old) = &old_path {
            path.push(':');
            path.push_str(old);
        }
        std::env::set_var("PATH", &path);
        let _marker = RestoreVar("GHOST_TEST_MARKER", std::env::var("GHOST_TEST_MARKER").ok());
        let _view = RestoreVar("GHOST_TEST_VIEW", std::env::var("GHOST_TEST_VIEW").ok());
        let _code = RestoreVar(
            "GHOST_TEST_MERGE_CODE",
            std::env::var("GHOST_TEST_MERGE_CODE").ok(),
        );
        let _err = RestoreVar(
            "GHOST_TEST_MERGE_ERR",
            std::env::var("GHOST_TEST_MERGE_ERR").ok(),
        );
        std::env::set_var("GHOST_TEST_MARKER", &marker);
        std::env::set_var("GHOST_TEST_VIEW", &view);
        std::env::remove_var("GHOST_TEST_MERGE_CODE");
        std::env::remove_var("GHOST_TEST_MERGE_ERR");

        let ready = |sha: &str| {
            format!(
                r#"{{"number":1,"title":"t","state":"OPEN","isDraft":false,"headRefOid":"{sha}","headRefName":"feature","baseRefName":"main","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED","statusCheckRollup":[{{"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"}}],"reviews":[],"comments":[],"author":{{"login":"me"}},"isCrossRepository":false,"maintainerCanModify":true}}"#
            )
        };
        std::fs::write(&view, ready("def")).unwrap();
        let _ = std::fs::remove_file(&marker);
        let (mut ctx, rx, _cmd) = merge_ctx(true);
        let merged = ctx
            .merge_now(1, "abc", crate::tui::MergeStyle::Squash)
            .unwrap();
        assert!(!merged, "a moved head must not merge");
        assert!(
            !marker.exists(),
            "merge_now must recheck the head SHA before gh pr merge"
        );
        assert!(rx.try_recv().is_err() || !marker.exists());

        std::fs::write(&view, ready("abc")).unwrap();
        let (mut ctx, rx, _cmd) = merge_ctx(true);
        let merged = ctx
            .merge_now(1, "abc", crate::tui::MergeStyle::Squash)
            .unwrap();
        assert!(merged);
        assert!(marker.exists(), "unchanged head still merges");
        let mut code = None;
        while let Ok(ev) = rx.try_recv() {
            if let crate::tui::UiEvent::Exit { code: c, .. } = ev {
                code = Some(c);
            }
        }
        assert_eq!(code, Some(0));

        let _ = std::fs::remove_file(&marker);
        std::env::set_var("GHOST_TEST_MERGE_CODE", "1");
        std::env::set_var(
            "GHOST_TEST_MERGE_ERR",
            "GraphQL: Squash merges are not allowed on this repository",
        );
        let (mut ctx, rx, _cmd) = merge_ctx(true);
        let merged = ctx
            .merge_now(1, "abc", crate::tui::MergeStyle::Squash)
            .unwrap();
        assert!(merged, "merge-yes failure leaves the loop");
        assert!(!marker.exists());
        let mut saw = None;
        while let Ok(ev) = rx.try_recv() {
            if let crate::tui::UiEvent::Exit { code, message } = ev {
                saw = Some((code, message));
            }
        }
        let (code, message) = saw.expect("merge-yes failure exits");
        assert_eq!(code, 1);
        assert!(
            message.contains("許可されていません"),
            "method rejection must name the flag and not guess another method: {message}"
        );
        assert!(message.contains("--merge-method") || message.contains("m "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_fetch_logs_a_502_and_does_not_exit() {
        let _lock = path_lock();
        let dir = std::env::temp_dir().join(format!(
            "ghost-auto-502-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_exe(&dir.join("gh"), "#!/bin/sh\necho 'HTTP 502' >&2\nexit 1\n");
        let old_path = std::env::var("PATH").ok();
        let _path = RestoreVar("PATH", old_path.clone());
        let mut path = dir.display().to_string();
        if let Some(old) = &old_path {
            path.push(':');
            path.push_str(old);
        }
        std::env::set_var("PATH", &path);

        let (mut ctx, rx, _cmd) = merge_ctx(false);
        ctx.spec.mode = super::RunMode::Auto;
        let state = dir.join("auto.json");
        let fetched = ctx.fetch_auto(&state);
        let message = match fetched {
            super::AutoFetch::Wait(message) => message,
            super::AutoFetch::Ready { .. } => panic!("a failing gh must wait, not stop"),
        };
        assert!(message.contains("502"), "{message}");
        ctx.note_auto_wait(message);
        assert!(!ctx.quit, "one 502 does not end --auto");
        let mut saw_exit = false;
        let mut saw_502 = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::tui::UiEvent::Exit { .. } | crate::tui::UiEvent::Fatal(_) => {
                    saw_exit = true;
                }
                crate::tui::UiEvent::Log(line) if line.text.contains("502") => saw_502 = true,
                _ => {}
            }
        }
        assert!(saw_502, "the 502 is logged");
        assert!(!saw_exit, "the 502 is not fatal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_auto_instances_do_not_take_the_same_issue() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-auto-slot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = dir.join("ghost-home");
        let main = home.join("work").join("acme__app");
        std::fs::create_dir_all(&main).unwrap();
        let slot = crate::workdir::slot_path(&home, "acme", "app", "issue-9");
        let held = crate::workdir::lock_slot(&slot).unwrap();
        let (mut ctx, rx, _cmd) = merge_ctx(false);
        ctx.spec.mode = super::RunMode::Auto;
        ctx.owner = "acme".into();
        ctx.repo = "app".into();
        ctx.main_clone = main.clone();
        ctx.workdir = main.clone();
        ctx.slot_dir = main;
        let state = dir.join("auto.json");
        let started = ctx.begin_auto_issue(9, &state, "").unwrap();
        assert!(!started, "a held issue-9 lock is skipped this round");
        assert!(ctx.auto_issue.is_none());
        assert!(
            ctx.setup_failures.is_empty(),
            "a busy lock is not a failure"
        );
        assert!(!state.exists(), "skipping does not record a failure");
        assert!(!ctx.quit);
        assert!(rx.try_recv().is_err() || !ctx.quit);
        drop(held);
        assert!(
            ctx.use_slot_inner("issue-9", false),
            "the issue is free once the other ghost leaves"
        );
        assert!(ctx.slot_lock.is_some());
        assert_ne!(ctx.workdir, ctx.main_clone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_issues_page_past_one_hundred() {
        let _lock = path_lock();
        let dir = std::env::temp_dir().join(format!(
            "ghost-auto-page-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let page1 = dir.join("page1.json");
        let page2 = dir.join("page2.json");
        let mut nodes = Vec::new();
        for number in 1..=100 {
            nodes.push(format!(
                r#"{{"number":{number},"title":"t{number}","createdAt":"2020-01-01T00:00:00Z","authorAssociation":"OWNER","author":{{"__typename":"User","login":"ada"}},"labels":{{"nodes":[]}}}}"#
            ));
        }
        std::fs::write(
            &page1,
            format!(
                r#"{{"data":{{"repository":{{"issues":{{"pageInfo":{{"hasNextPage":true,"endCursor":"PAGE2"}},"nodes":[{}]}}}}}}}}"#,
                nodes.join(",")
            ),
        )
        .unwrap();
        std::fs::write(
            &page2,
            r#"{"data":{"repository":{"issues":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"number":250,"title":"later","createdAt":"2024-01-01T00:00:00Z","authorAssociation":"COLLABORATOR","author":{"__typename":"User","login":"bea"},"labels":{"nodes":[]}}]}}}}"#,
        )
        .unwrap();
        write_exe(
            &dir.join("gh"),
            "#!/bin/sh\ncase \" $* \" in\n  *cursor=PAGE2*) cat \"$GHOST_TEST_PAGE2\"; exit 0 ;;\nesac\ncat \"$GHOST_TEST_PAGE1\"\nexit 0\n",
        );
        let old_path = std::env::var("PATH").ok();
        let _path = RestoreVar("PATH", old_path.clone());
        let mut path = dir.display().to_string();
        if let Some(old) = &old_path {
            path.push(':');
            path.push_str(old);
        }
        std::env::set_var("PATH", &path);
        let _page1 = RestoreVar("GHOST_TEST_PAGE1", std::env::var("GHOST_TEST_PAGE1").ok());
        let _page2 = RestoreVar("GHOST_TEST_PAGE2", std::env::var("GHOST_TEST_PAGE2").ok());
        std::env::set_var("GHOST_TEST_PAGE1", &page1);
        std::env::set_var("GHOST_TEST_PAGE2", &page2);
        let rows = crate::github::Gh::new("acme/app").auto_issues().unwrap();
        assert!(
            rows.len() > 100,
            "the second page is fetched, got {}",
            rows.len()
        );
        assert_eq!(rows.last().map(|row| row.number), Some(250));
        assert_eq!(
            rows.last().map(|row| row.association.as_str()),
            Some("COLLABORATOR")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `auto_loop` with a stub `gh` and a stub agent. Issue #502's title mentions
    /// timeout, connection, and rate limit; the missing pull request is not a GitHub blip.
    #[test]
    fn auto_loop_with_stub_gh_and_agent_does_not_retry_a_titled_timeout() {
        let _lock = path_lock();
        let dir = std::env::temp_dir().join(format!(
            "ghost-auto-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = dir.join("ghost-home");
        let main = home.join("work").join("acme__app");
        std::fs::create_dir_all(&main).unwrap();
        let state_root = dir.join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        let gh = dir.join("gh");
        write_exe(
            &gh,
            r#"#!/bin/sh
case " $* " in
  *" graphql "*)
    case " $* " in
      *pullRequests*)
        printf '%s\n' '{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}'
        ;;
      *)
        printf '%s\n' '{"data":{"repository":{"issues":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"number":502,"title":"fix timeout connection rate limit","createdAt":"2020-01-01T00:00:00Z","authorAssociation":"OWNER","author":{"__typename":"User","login":"ada"},"labels":{"nodes":[]}}]}}}}'
        ;;
    esac
    exit 0
    ;;
  *" repo view "*)
    printf '%s\n' '{"defaultBranchRef":{"name":"main"}}'
    exit 0
    ;;
  *" closedByPullRequestsReferences "*)
    printf '%s\n' '{}'
    exit 0
    ;;
  *" issue view "*)
    printf '%s\n' '{"number":502,"title":"fix timeout connection rate limit","body":"connection rate limit","state":"OPEN","labels":[]}'
    exit 0
    ;;
esac
printf '%s\n' '[]'
exit 0
"#,
        );
        let agent = dir.join("agent");
        write_exe(&agent, "#!/bin/sh\ncat >/dev/null\nexit 0\n");
        let old_path = std::env::var("PATH").ok();
        let _path = RestoreVar("PATH", old_path.clone());
        let mut path = dir.display().to_string();
        if let Some(old) = &old_path {
            path.push(':');
            path.push_str(old);
        }
        std::env::set_var("PATH", &path);

        let (mut ctx, rx, cmd_tx) = merge_ctx(false);
        ctx.spec.mode = super::RunMode::Auto;
        ctx.spec.interval = Duration::from_secs(30);
        ctx.spec.lang = Lang::En;
        ctx.spec.trust = crate::watch::CommentTrust::All;
        ctx.spec.trust_issues = crate::watch::CommentTrust::Collaborators;
        ctx.owner = "acme".into();
        ctx.repo = "app".into();
        ctx.slug = "acme/app".into();
        ctx.gh = crate::github::Gh::new("acme/app");
        ctx.main_clone = main.clone();
        ctx.workdir = main.clone();
        ctx.slot_dir = main;
        ctx.state_root = state_root.clone();
        ctx.agent.set_stub(agent);
        let state_file = state_root.join("acme__app").join("auto.json");
        let watch = state_file.clone();
        let quit_tx = cmd_tx.clone();
        let watcher = std::thread::spawn(move || {
            for _ in 0..400 {
                if let Ok(text) = std::fs::read_to_string(&watch) {
                    if text.contains("502") {
                        let _ = quit_tx.send(crate::tui::UiCmd::Quit);
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = quit_tx.send(crate::tui::UiCmd::Quit);
        });
        ctx.auto_loop().unwrap();
        watcher.join().unwrap();
        let raw = std::fs::read_to_string(&state_file).expect("auto.json records the miss");
        let stored: crate::state::AutoState = serde_json::from_str(&raw).unwrap();
        let row = stored
            .failed
            .iter()
            .find(|row| row.number == 502)
            .expect("issue 502 is recorded");
        assert!(
            !row.transient,
            "title words and #502 must not be a transient retry: {raw}"
        );
        assert!(row.reason.contains("timeout"), "{raw}");
        assert!(row.reason.contains("502"), "{raw}");
        assert!(row.reason.contains("connection"), "{raw}");
        assert!(
            row.reason.contains("rate-limit") || row.reason.contains("rate limit"),
            "{raw}"
        );
        let mut saw_fatal = false;
        let mut saw_temporary = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                crate::tui::UiEvent::Fatal(_) => saw_fatal = true,
                crate::tui::UiEvent::Exit { code, .. } => assert_eq!(code, 0),
                crate::tui::UiEvent::Log(line) if line.text.contains("temporarily") => {
                    saw_temporary = true;
                }
                _ => {}
            }
        }
        assert!(!saw_fatal, "--auto does not exit on this miss");
        assert!(!saw_temporary, "the miss is not retried as a GitHub blip");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
