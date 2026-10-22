//! Poll GitHub with `gh`. Talk to the agent only when something changed,
//! always in the one session started for this run.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::agent::{agent_on_path, describe_launch, pick_agent, AgentKind, AgentSession};
use crate::github::Gh;
use crate::preflight::{evaluate, failure_message, AuthProbe, RepoProbe};
use crate::prompt::{self, truncate};
use crate::shell::which;
use crate::state::{self, GhostState, SessionSlot};
use crate::target::GhTarget;
use crate::text::{format_countdown, short_sha};
use crate::tui::{self, LogKind, LogLine, Side, Status, UiCmd, UiEvent};
use crate::watch::{decide_code, decide_review, CodeAction, MaxTurns, Note, PrSnap, ReviewAction};
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
}

pub fn run(spec: RunSpec) -> Result<i32> {
    let header = spec.header.clone();
    tui::run(header, move |tx, rx| {
        if let Err(err) = babysit(spec, tx.clone(), rx) {
            let message = format!("{err:#}");
            let _ = tx.send(UiEvent::Fatal(message.clone()));
            let _ = tx.send(UiEvent::Exit { code: 1, message });
        }
    })
}

fn babysit(spec: RunSpec, tx: Sender<UiEvent>, rx: Receiver<UiCmd>) -> Result<()> {
    let mut status = Status {
        mode: mode_name(spec.mode).into(),
        target: spec.target.display_url(),
        phase: "starting".into(),
        ..Status::default()
    };
    let _ = tx.send(UiEvent::Status(status.clone()));

    let login = match run_preflight(&spec.target.slug()) {
        Ok(login) => login,
        Err(message) => {
            let _ = tx.send(UiEvent::Fatal(message.clone()));
            wait_quit(&rx);
            let _ = tx.send(UiEvent::Exit { code: 1, message });
            return Ok(());
        }
    };
    log(
        &tx,
        LogKind::System,
        match &login {
            Some(name) => format!("gh ok · {name}"),
            None => "gh ok".into(),
        },
    );

    let home = workdir::ghost_home()?;
    let (workdir, how) = workdir::prepare(&home, spec.target.owner(), spec.target.repo())?;
    log(
        &tx,
        LogKind::System,
        format!("workdir {} ({how})", workdir.display()),
    );

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
        spec,
        tx,
        rx,
        turns: 0,
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
    agent: AgentSession,
    turns: u32,
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
                    self.stop(reason.exit_code(), reason.message(number));
                    return Ok(());
                }
                Ok(ReviewAction::Idle) => self.note_idle(&snap.head_sha),
                Ok(ReviewAction::Review { .. }) => {
                    self.checkout(Checkout::Pr(number))?;
                    if self.quit {
                        return Ok(());
                    }
                    let diff = self.load_diff(
                        number,
                        stored.last_reviewed_sha.as_deref(),
                        &snap.head_sha,
                    )?;
                    let notes = std::mem::take(&mut self.queued);
                    let task = match self.agent.session_id() {
                        Some(_) => prompt::review_followup(
                            &self.slug,
                            number,
                            stored.last_reviewed_sha.as_deref().unwrap_or("unreviewed"),
                            &snap.head_sha,
                            &self.workdir,
                            &diff,
                            &notes,
                        ),
                        None => prompt::review_initial(
                            &self.slug,
                            number,
                            &snap.head_sha,
                            &self.workdir,
                            &diff,
                            &notes,
                        ),
                    };
                    self.dispatch_task(&task)?;
                    if self.quit {
                        return Ok(());
                    }
                    if !self.interrupted {
                        stored.last_reviewed_sha = Some(snap.head_sha.clone());
                        self.remember(&mut stored, SessionSlot::Review);
                        stored.save(&path)?;
                        self.log(format!(
                            "recorded reviewed SHA {}",
                            short_sha(&snap.head_sha)
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
        self.push_status();
        loop {
            if self.quit {
                return Ok(());
            }
            let snap = match self.poll_pr(number)? {
                Some(snap) => snap,
                None => return Ok(()),
            };
            match decide_code(&snap, &seen, self.turns, self.spec.max_turns) {
                Err(MaxTurns) => {
                    self.stop(3, self.max_message());
                    return Ok(());
                }
                Ok(CodeAction::Stop(reason)) => {
                    self.stop(reason.exit_code(), reason.message(number));
                    return Ok(());
                }
                Ok(CodeAction::Idle) => self.note_code_idle(&snap),
                Ok(CodeAction::Fix {
                    ci_failed,
                    new_notes,
                }) => {
                    self.announce_fix(ci_failed, &snap, &new_notes);
                    self.checkout(Checkout::Pr(number))?;
                    if self.quit {
                        return Ok(());
                    }
                    let user_notes = std::mem::take(&mut self.queued);
                    let task = if self.agent.session_id().is_some() {
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
                        )
                    };
                    self.dispatch_task(&task)?;
                    if self.quit {
                        return Ok(());
                    }
                    if !self.interrupted {
                        if let Ok(fresh) = self.gh.pr(number) {
                            for note in fresh.notes {
                                seen.insert(note.id);
                            }
                        }
                        for note in &new_notes {
                            seen.insert(note.id.clone());
                        }
                        stored.seen_comment_ids = seen.iter().cloned().collect();
                        self.remember(&mut stored, SessionSlot::Code);
                        stored.save(&path)?;
                        self.log("recorded the comments this session has seen");
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
        let branch = workdir::issue_branch(number);
        if let Some(pr) = self.gh.find_issue_pr(number, &branch)? {
            self.log(format!(
                "found existing PR #{pr} for issue #{number}; not opening a duplicate"
            ));
            return self.confirm_and_code(pr);
        }
        let issue = self.gh.issue(number)?;
        let base = self.gh.default_branch()?;
        let ipath = state::issue_state_path(&self.state_root, &self.owner, &self.repo, number);
        let mut istate = GhostState::load(&ipath)?;
        self.agent
            .seed(istate.session_for(SessionSlot::Code, self.agent.kind()));
        self.checkout(Checkout::Issue {
            number,
            base: base.clone(),
        })?;
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
        );
        if self.agent.session_id().is_some() {
            task = format!(
                "Follow-up in this same session. Continue implementing issue #{number} on `{branch}` and make sure a pull request with `Closes #{number}` exists. Do not open a duplicate. Do not merge.\n\n{task}"
            );
        }
        self.dispatch_task(&task)?;
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
        let ppath = state::pr_state_path(&self.state_root, &self.owner, &self.repo, pr);
        let mut pstate = GhostState::load(&ppath)?;
        self.remember(&mut pstate, SessionSlot::Code);
        pstate.save(&ppath)?;
        self.log(format!(
            "opened PR #{pr} · keeping this {} session",
            self.agent.kind()
        ));
        self.confirm_and_code(pr)
    }

    fn confirm_and_code(&mut self, pr: u64) -> Result<()> {
        let message = format!("Continue as --code on PR #{pr}? [Y/n]");
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
            self.log(format!(
                "head {} unchanged since the last review; not calling the agent",
                short_sha(sha)
            ));
            self.last_idle_sha = Some(sha.to_string());
        }
    }

    fn note_code_idle(&mut self, snap: &PrSnap) {
        let key = format!("{}:{}", snap.head_sha, snap.ci_label);
        if self.last_idle_sha.as_deref() != Some(&key) {
            self.log(format!(
                "nothing new on PR #{} (CI {}); not calling the agent",
                snap.number, snap.ci_label
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

    fn checkout(&mut self, checkout: Checkout) -> Result<()> {
        let note = workdir::sync_and_checkout(&self.workdir, &self.slug, checkout)?;
        if !note.is_empty() {
            self.log(note);
        }
        Ok(())
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

    fn dispatch_task(&mut self, prompt: &str) -> Result<()> {
        if self.quit {
            return Ok(());
        }
        if self.turns >= self.spec.max_turns {
            self.stop(3, self.max_message());
            return Ok(());
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
        self.refresh_session_status();
        self.push_status();
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
                    }
                }
            }
            if self.agent.turn_finished() {
                self.log(format!(
                    "agent turn finished ({}/{})",
                    self.turns, self.spec.max_turns
                ));
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
            let prompt = format!(
                "The user typed this into ghost. Stay in this same session. Use the `gh` CLI for any GitHub action. Do not merge.\n\n{text}"
            );
            self.dispatch_task(&prompt)?;
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
        let prompt = format!(
            "The user typed this into ghost while the previous turn was running. Stay in this same session. Use the `gh` CLI for any GitHub action. Do not merge.\n\n{}",
            notes.join("\n\n")
        );
        self.dispatch_task(&prompt)
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
        format!(
            "Hit the max of {} AI turns without the PR becoming merge-ready. Stopping so --review and --code cannot ping-pong forever. State was saved; re-run ghost to continue.",
            self.spec.max_turns
        )
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

fn run_preflight(slug: &str) -> Result<Option<String>, String> {
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
        Ok(ok) => Ok(ok.login),
        Err(kind) => Err(failure_message(&kind, &auth, repo.as_ref(), slug)),
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

fn short_id(id: &str) -> &str {
    let n = id.len().min(8);
    &id[..n]
}
