//! One coding-agent session for the whole ghost run.
//!
//! The first task starts the session. Every later task is a follow-up in that
//! same session: Claude and Grok `--resume <id>`, Codex `exec resume <id>`.
//! A follow-up never starts a second conversation. Claude stays as one process
//! and takes further prompts on stdin when that process is still alive; if it
//! exits, ghost resumes the same session id.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::Value;

use crate::shell::{self, which, PromptMode};
use crate::text::strip_ansi;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Codex,
    Claude,
    Grok,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
        }
    }

    pub fn resume_syntax(self) -> &'static str {
        match self {
            Self::Codex => "codex exec resume <id>",
            Self::Claude => "claude --resume <id>",
            Self::Grok => "grok --resume <id>",
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// First turn. Claude and Grok are pinned to `preset_session_id`.
    /// Codex reports its own thread id on stdout.
    Start { preset_session_id: Option<String> },
    /// Continue the session that is already running. Not a new agent.
    Resume { session_id: String },
}

/// Decide whether the next turn starts the session or resumes it.
/// A non-empty id resumes only when this process already holds that id
/// (a follow-up, or text the user sent with `i`). A launch does not resume
/// an id loaded from disk.
pub fn plan_launch(kind: AgentKind, existing: Option<&str>, new_uuid: &str) -> Launch {
    if let Some(id) = existing {
        if !id.is_empty() {
            return Launch::Resume {
                session_id: id.to_string(),
            };
        }
    }
    match kind {
        AgentKind::Codex => Launch::Start {
            preset_session_id: None,
        },
        AgentKind::Claude | AgentKind::Grok => Launch::Start {
            preset_session_id: Some(new_uuid.to_string()),
        },
    }
}

pub fn describe_launch(kind: AgentKind, launch: &Launch) -> String {
    match launch {
        Launch::Start {
            preset_session_id: Some(id),
        } => format!("starting {kind} session {id}"),
        Launch::Start { .. } => format!("starting {kind} session"),
        Launch::Resume { session_id } => {
            format!(
                "follow-up in the same {kind} session {session_id} ({})",
                kind.resume_syntax()
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Newline-delimited JSON user messages. Leave stdin open.
    StreamJson,
    /// Prompt bytes, then close stdin so the CLI starts.
    StdinClose,
    /// Prompt already written to `--prompt-file`.
    PromptFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    pub program: &'static str,
    pub args: Vec<String>,
    pub delivery: Delivery,
}

pub fn command_for(
    kind: AgentKind,
    launch: &Launch,
    workdir: &Path,
    prompt_file: &Path,
) -> Result<AgentCommand, String> {
    let dir = workdir.display().to_string();
    let file = prompt_file.display().to_string();
    let cmd = match (kind, launch) {
        (
            AgentKind::Claude,
            Launch::Start {
                preset_session_id: Some(id),
            },
        ) => AgentCommand {
            program: "claude",
            args: vec![
                "--print".into(),
                "--dangerously-skip-permissions".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--session-id".into(),
                id.clone(),
            ],
            delivery: Delivery::StreamJson,
        },
        // `--resume` continues the id. `--session-id` would start a different
        // conversation, and `--fork-session` would copy it. No cwd flag; the
        // child process cwd is the worktree. stream-json still needs `--verbose`.
        (AgentKind::Claude, Launch::Resume { session_id }) => AgentCommand {
            program: "claude",
            args: vec![
                "--print".into(),
                "--dangerously-skip-permissions".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--resume".into(),
                session_id.clone(),
            ],
            delivery: Delivery::StreamJson,
        },
        (AgentKind::Codex, Launch::Start { .. }) => AgentCommand {
            program: "codex",
            args: vec![
                "exec".into(),
                "--json".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--skip-git-repo-check".into(),
                "-C".into(),
                dir,
                "-".into(),
            ],
            delivery: Delivery::StdinClose,
        },
        // `codex exec resume` does not accept `-C`/`--cd` (it is not on ResumeArgs
        // and is not an exec global). The child process cwd is the worktree.
        // `--json`, the bypass flag, and `--skip-git-repo-check` are exec globals,
        // so they go before `resume`. Do not pass `-s`/`--sandbox`; the bypass
        // flag is the accepted global, not a `-c` override.
        (AgentKind::Codex, Launch::Resume { session_id }) => AgentCommand {
            program: "codex",
            args: vec![
                "exec".into(),
                "--json".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--skip-git-repo-check".into(),
                "resume".into(),
                session_id.clone(),
                "-".into(),
            ],
            delivery: Delivery::StdinClose,
        },
        (
            AgentKind::Grok,
            Launch::Start {
                preset_session_id: Some(id),
            },
        ) => AgentCommand {
            program: "grok",
            args: vec![
                "--prompt-file".into(),
                file,
                "--session-id".into(),
                id.clone(),
                "--output-format".into(),
                "streaming-json".into(),
                "--yolo".into(),
                "--cwd".into(),
                dir,
            ],
            delivery: Delivery::PromptFile,
        },
        // `--resume` is a top-level flag, not a subcommand. Docs accept
        // `--cwd`, `--yolo`, `--prompt-file`, and `--output-format` with it.
        // `--session-id` creates a new session, so it stays off this argv.
        // The child process cwd is also the worktree.
        (AgentKind::Grok, Launch::Resume { session_id }) => AgentCommand {
            program: "grok",
            args: vec![
                "--prompt-file".into(),
                file,
                "--resume".into(),
                session_id.clone(),
                "--output-format".into(),
                "streaming-json".into(),
                "--yolo".into(),
                "--cwd".into(),
                dir,
            ],
            delivery: Delivery::PromptFile,
        },
        _ => return Err(format!("internal: {kind} start is missing a session id")),
    };
    Ok(cmd)
}

pub fn claude_user_line(prompt: &str) -> String {
    let v = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": prompt }]
        }
    });
    format!("{v}\n")
}

pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    let mut file = fs::File::open("/dev/urandom").expect("open /dev/urandom");
    file.read_exact(&mut bytes).expect("read /dev/urandom");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

pub fn extract_session_id(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    for key in ["thread_id", "session_id", "sessionId"] {
        if let Some(id) = v.get(key).and_then(|x| x.as_str()) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

pub fn event_is_error(kind: AgentKind, line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        AgentKind::Claude => v.get("is_error").and_then(|b| b.as_bool()) == Some(true),
        AgentKind::Codex => ty == "turn.failed",
        AgentKind::Grok => {
            ty == "error" || v.get("is_error").and_then(|b| b.as_bool()) == Some(true)
        }
    }
}

/// A resume that dies in CLI parsing, or whose session does not exist,
/// never became an agent turn. A real failure (`turn.failed`, exit 1) still counts.
pub fn counts_as_agent_turn(launch: &Launch, exit_code: Option<i32>, output: &str) -> bool {
    if !matches!(launch, Launch::Resume { .. }) {
        return true;
    }
    !(cli_usage_failure(exit_code, output) || resume_unavailable(exit_code, output))
}

/// Codex has no rollout for this thread id, or the session is otherwise unknown.
/// Exit 0 and a real turn result are not this case.
pub fn resume_unavailable(exit_code: Option<i32>, output: &str) -> bool {
    if exit_code == Some(0) || has_agent_result_event(output) {
        return false;
    }
    let text = output.to_ascii_lowercase();
    text.contains("no rollout found")
        || text.contains("thread/resume")
        || text.contains("unknown session")
        || text.contains("session not found")
        || text.contains("no such session")
        || text.contains("-32600")
}

fn cli_usage_failure(exit_code: Option<i32>, output: &str) -> bool {
    if has_agent_result_event(output) {
        return false;
    }
    let text = output.to_ascii_lowercase();
    let clap_usage = text.contains("unexpected argument")
        || text.contains("unexpected option")
        || (text.contains("error:") && text.contains("usage:"));
    exit_code == Some(2) || (exit_code.is_some_and(|code| code != 0) && clap_usage)
}

fn has_agent_result_event(output: &str) -> bool {
    let compact = output.replace(' ', "");
    compact.contains("\"type\":\"turn.completed\"")
        || compact.contains("\"type\":\"turn.failed\"")
        || compact.contains("\"type\":\"result\"")
        || compact.contains("\"type\":\"end\"")
}

/// Record a review SHA or seen comments only after a finished successful turn.
pub fn record_progress(
    interrupted: bool,
    exit_code: Option<i32>,
    clean_live: bool,
    saw_error: bool,
) -> bool {
    if interrupted || saw_error {
        return false;
    }
    exit_code == Some(0) || (exit_code.is_none() && clean_live)
}

pub fn is_turn_end(kind: AgentKind, line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return false;
    };
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        AgentKind::Claude => ty == "result",
        AgentKind::Codex => ty == "turn.completed" || ty == "turn.failed",
        AgentKind::Grok => ty == "end" || ty == "result",
    }
}

/// Lines worth showing in the TUI. Metadata events become nothing.
pub fn present_line(line: &str) -> Vec<String> {
    let clean = strip_ansi(line);
    let clean = clean.trim_end_matches(['\r', '\n']);
    if clean.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<Value>(clean) else {
        return vec![clean.to_string()];
    };
    present_event(&v)
}

fn present_event(v: &Value) -> Vec<String> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "system" | "thread.started" | "turn.started" | "turn.completed" | "turn.failed" | "end"
        | "result" => Vec::new(),
        "stream_event" => v
            .pointer("/event/delta/text")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "assistant" => texts_from_message(v.get("message").unwrap_or(v)),
        "error" => v
            .get("message")
            .and_then(|t| t.as_str())
            .or_else(|| v.get("error").and_then(|t| t.as_str()))
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        _ => {
            if let Some(item) = v.get("item") {
                return present_item(item);
            }
            if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    return vec![text.to_string()];
                }
            }
            Vec::new()
        }
    }
}

fn texts_from_message(msg: &Value) -> Vec<String> {
    let content = msg.get("content");
    let Some(content) = content else {
        return msg
            .get("text")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();
    };
    if let Some(text) = content.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in blocks {
        let bty = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match bty {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
            "tool_use" => {
                let name = block.get("name").and_then(|t| t.as_str()).unwrap_or("tool");
                let cmd = block
                    .pointer("/input/command")
                    .and_then(|t| t.as_str())
                    .or_else(|| block.pointer("/input/cmd").and_then(|t| t.as_str()));
                if let Some(cmd) = cmd {
                    out.push(format!("$ {cmd}"));
                } else {
                    out.push(format!("tool {name}"));
                }
            }
            _ => {}
        }
    }
    out
}

fn present_item(item: &Value) -> Vec<String> {
    let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "agent_message" => item
            .get("text")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(split_keep)
            .unwrap_or_default(),
        "command_execution" => item
            .get("command")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| vec![format!("$ {s}")])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn split_keep(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn pick_agent(
    requested: Option<AgentKind>,
    mut available: impl FnMut(AgentKind) -> bool,
) -> Result<AgentKind, String> {
    if let Some(kind) = requested {
        if available(kind) {
            Ok(kind)
        } else {
            Err(format!(
                "`{kind}` is not on PATH. Install it, or pass --agent with one that is."
            ))
        }
    } else {
        for kind in [AgentKind::Codex, AgentKind::Claude, AgentKind::Grok] {
            if available(kind) {
                return Ok(kind);
            }
        }
        Err("no coding agent on PATH. Install codex, claude, or grok (tried in that order).".into())
    }
}

pub fn agent_on_path(kind: AgentKind) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    which(kind.as_str(), &path).is_some()
}

/// Prompt files are per process and per slot. Two ghost processes must not
/// share `~/.ghost/prompts/codex.md`.
pub fn prompt_directory(home: &Path, slot: &str) -> PathBuf {
    let raw = slot.trim();
    let slot = if raw.is_empty() { "main" } else { raw };
    let slot = slot.replace(['/', ' '], "-");
    home.join("prompts")
        .join(format!("pid{}-{slot}", std::process::id()))
}

/// Directory the agent process can actually start in. A missing worktree
/// falls back to `~/.ghost/work` (created when needed).
pub fn existing_cwd(preferred: &Path, fallback: &Path) -> PathBuf {
    if preferred.is_dir() {
        return preferred.to_path_buf();
    }
    if !fallback.is_dir() {
        let _ = fs::create_dir_all(fallback);
    }
    if fallback.is_dir() {
        fallback.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

/// Where `git push` from the agent is allowed to go. Review is read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    pub readonly: bool,
    pub remote: String,
    pub refspec: String,
    pub reason: String,
}

/// The long-lived handle ghost keeps from the first task until exit.
pub struct AgentSession {
    kind: AgentKind,
    workdir: PathBuf,
    ghost_home: PathBuf,
    fallback_dir: PathBuf,
    prompt_dir: PathBuf,
    session_id: Option<String>,
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    rx: Option<Receiver<String>>,
    readers: Arc<AtomicUsize>,
    pending: VecDeque<String>,
    turn_open: bool,
    exited: bool,
    last_exit: Option<i32>,
    last_output: String,
    clean_end: bool,
    saw_error: bool,
    /// The child closed stdin before reading the prompt. The turn failed;
    /// it is not a fatal error for ghost.
    broken_pipe: bool,
    push_plan: Option<PushPlan>,
    stub: Option<PathBuf>,
    /// File the `gh` wrapper appends when the agent runs `gh issue create`.
    created_issues: Option<PathBuf>,
}

impl AgentSession {
    pub fn new(kind: AgentKind, workdir: PathBuf, ghost_home: &Path, slot: &str) -> Self {
        Self {
            kind,
            workdir,
            ghost_home: ghost_home.to_path_buf(),
            fallback_dir: ghost_home.join("work"),
            prompt_dir: prompt_directory(ghost_home, slot),
            session_id: None,
            child: None,
            stdin: None,
            rx: None,
            readers: Arc::new(AtomicUsize::new(0)),
            pending: VecDeque::new(),
            turn_open: false,
            exited: false,
            last_exit: None,
            last_output: String::new(),
            clean_end: false,
            saw_error: false,
            broken_pipe: false,
            push_plan: None,
            stub: None,
            created_issues: None,
        }
    }

    pub fn set_created_issues(&mut self, path: PathBuf) {
        self.created_issues = Some(path);
    }

    pub fn set_workdir(&mut self, dir: PathBuf) {
        self.workdir = dir;
    }

    pub fn set_prompt_slot(&mut self, slot: &str) {
        self.prompt_dir = prompt_directory(&self.ghost_home, slot);
    }

    pub fn workdir_is_dir(&self) -> bool {
        self.workdir.is_dir()
    }

    pub fn fallback_dir(&self) -> PathBuf {
        self.fallback_dir.clone()
    }

    pub fn set_push_plan(&mut self, plan: PushPlan) {
        self.push_plan = Some(plan);
    }

    /// True when the last prompt write got EPIPE. Cleared on the next dispatch.
    pub fn prompt_was_refused(&self) -> bool {
        self.broken_pipe
    }

    /// Test hook: run this program instead of codex/claude/grok.
    pub fn set_stub(&mut self, program: PathBuf) {
        self.stub = Some(program);
    }

    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit
    }

    /// Stdout and stderr captured for this launch, used to spot CLI usage errors.
    pub fn recent_output(&self) -> &str {
        &self.last_output
    }

    pub fn clean_live(&self) -> bool {
        self.clean_end && !self.saw_error && self.last_exit.is_none()
    }

    pub fn saw_error(&self) -> bool {
        self.saw_error
    }

    /// Drop the saved id so the next task starts a new session.
    pub fn forget_session(&mut self) {
        self.kill_child(9);
        self.session_id = None;
        self.turn_open = false;
    }

    pub fn kind(&self) -> AgentKind {
        self.kind
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Restore an id from disk so the next task resumes instead of starting over.
    pub fn seed(&mut self, id: Option<String>) {
        if self.session_id.is_none() {
            if let Some(id) = id {
                if !id.is_empty() {
                    self.session_id = Some(id);
                }
            }
        }
    }

    pub fn turn_open(&self) -> bool {
        self.turn_open
    }

    pub fn accepts_live_input(&self) -> bool {
        self.kind == AgentKind::Claude && self.stdin.is_some() && !self.exited
    }

    pub fn dispatch(&mut self, prompt: &str) -> Result<Launch> {
        self.last_exit = None;
        self.last_output.clear();
        self.clean_end = false;
        self.saw_error = false;
        self.broken_pipe = false;
        let launch = plan_launch(self.kind, self.session_id.as_deref(), &new_session_id());
        if self.kind == AgentKind::Claude && self.alive() && matches!(launch, Launch::Resume { .. })
        {
            self.write_claude(prompt)?;
            self.turn_open = true;
            self.exited = false;
            return Ok(launch);
        }
        self.kill_child(9);
        self.spawn(&launch, prompt)?;
        if let Launch::Start {
            preset_session_id: Some(id),
        } = &launch
        {
            self.session_id = Some(id.clone());
        }
        self.turn_open = true;
        self.exited = false;
        Ok(launch)
    }

    /// User text while Claude is still up. Codex and Grok cannot take stdin
    /// mid-turn; the caller queues those and dispatches a follow-up later.
    pub fn push_live(&mut self, prompt: &str) -> Result<()> {
        if !self.accepts_live_input() {
            anyhow::bail!("agent is not accepting live input");
        }
        self.write_claude(prompt)
    }

    pub fn poll_lines(&mut self) -> Vec<String> {
        self.drain_rx();
        let mut shown = Vec::new();
        while let Some(raw) = self.pending.pop_front() {
            self.remember_output(&raw);
            if let Some(id) = extract_session_id(&raw) {
                self.session_id = Some(id);
            }
            if is_turn_end(self.kind, &raw) {
                self.turn_open = false;
                self.clean_end = true;
                if event_is_error(self.kind, &raw) {
                    self.saw_error = true;
                }
            }
            shown.extend(present_line(&raw));
        }
        shown
    }

    pub fn turn_finished(&mut self) -> bool {
        let _ = self.poll_lines();
        self.reap();
        if self.exited && self.readers.load(Ordering::SeqCst) == 0 && self.rx_empty() {
            self.turn_open = false;
            return true;
        }
        // Claude stays up after a result event. Codex and Grok exit; their
        // status is recorded above once the pipes drain.
        if self.kind == AgentKind::Claude && !self.turn_open && !self.exited && self.child.is_some()
        {
            return true;
        }
        false
    }

    pub fn interrupt(&mut self) {
        if let Some(pid) = self.child.as_ref().map(|c| c.id()) {
            signal_group(pid, 2);
        }
    }

    pub fn stop(&mut self) {
        self.kill_child(9);
        self.turn_open = false;
    }

    fn write_claude(&mut self, prompt: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().context("claude session has no stdin")?;
        if write_ignoring_pipe(stdin, claude_user_line(prompt).as_bytes())? {
            self.broken_pipe = true;
            self.stdin = None;
        }
        Ok(())
    }

    fn spawn(&mut self, launch: &Launch, prompt: &str) -> Result<()> {
        // A missing slot must not make `current_dir` fail with ENOENT.
        // The agent runs in ~/.ghost/work and the prompt carries the failed step.
        self.workdir = existing_cwd(&self.workdir, &self.fallback_dir);
        fs::create_dir_all(&self.prompt_dir)?;
        let file_key = match launch {
            Launch::Resume { session_id } => session_id.clone(),
            Launch::Start {
                preset_session_id: Some(id),
            } => id.clone(),
            Launch::Start { .. } => "codex".into(),
        };
        let prompt_file = self.prompt_dir.join(format!("{file_key}.md"));
        fs::write(&prompt_file, prompt).context("write agent prompt")?;

        let spec = command_for(self.kind, launch, &self.workdir, &prompt_file)
            .map_err(|e| anyhow::anyhow!(e))?;
        let program = self
            .stub
            .clone()
            .unwrap_or_else(|| PathBuf::from(spec.program));
        let mut cmd = Command::new(&program);
        cmd.args(&spec.args);
        cmd.current_dir(&self.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let existing_ssh = std::env::var("GIT_SSH_COMMAND").ok();
        for (key, value) in shell::prompt_env(
            PromptMode::Batch,
            shell::ssh_key().as_deref(),
            existing_ssh.as_deref(),
        ) {
            cmd.env(key, value);
        }
        let parent_path = std::env::var("PATH").unwrap_or_default();
        let bin = self.prompt_dir.join("bin");
        let mut guard_dir = None;
        if let Some(real) = which("gh", &parent_path) {
            if install_gh_guard(&bin).is_ok() {
                cmd.env("GHOST_REAL_GH", real);
                guard_dir = Some(bin.display().to_string());
            }
        }
        if let Some(real) = which("git", &parent_path) {
            if install_git_guard(&bin).is_ok() {
                cmd.env("GHOST_REAL_GIT", real);
                guard_dir = Some(bin.display().to_string());
            }
        }
        if let Some(dir) = guard_dir {
            let mut path = dir;
            if !parent_path.is_empty() {
                path.push(':');
                path.push_str(&parent_path);
            }
            cmd.env("PATH", path);
        }
        if let Some(path) = &self.created_issues {
            cmd.env("GHOST_CREATED_ISSUES", path);
        }
        if let Some(plan) = &self.push_plan {
            if plan.readonly {
                cmd.env("GHOST_PUSH_MODE", "readonly");
                cmd.env("GHOST_PUSH_REASON", &plan.reason);
            } else {
                cmd.env("GHOST_PUSH_MODE", "push");
                cmd.env("GHOST_PUSH_REMOTE", &plan.remote);
                cmd.env("GHOST_PUSH_REFSPEC", &plan.refspec);
            }
        }
        unsafe {
            cmd.pre_exec(|| {
                if setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", spec.program))?;
        let stdout = child.stdout.take().context("stdout")?;
        let stderr = child.stderr.take().context("stderr")?;
        let (rx, readers) = spawn_readers(stdout, stderr);
        track_agent_pgid(child.id());
        let mut stdin = child.stdin.take().context("stdin")?;
        match spec.delivery {
            Delivery::StreamJson => {
                if write_ignoring_pipe(&mut stdin, claude_user_line(prompt).as_bytes())? {
                    self.broken_pipe = true;
                    drop(stdin);
                    self.stdin = None;
                } else {
                    self.stdin = Some(stdin);
                }
            }
            Delivery::StdinClose => {
                let mut bytes = prompt.as_bytes().to_vec();
                if !prompt.ends_with('\n') {
                    bytes.push(b'\n');
                }
                if write_ignoring_pipe(&mut stdin, &bytes)? {
                    self.broken_pipe = true;
                }
                drop(stdin);
                self.stdin = None;
            }
            Delivery::PromptFile => {
                drop(stdin);
                self.stdin = None;
            }
        }
        self.child = Some(child);
        self.rx = Some(rx);
        self.readers = readers;
        Ok(())
    }

    fn remember_output(&mut self, line: &str) {
        const CAP: usize = 8_000;
        self.last_output.push_str(line);
        if self.last_output.len() > CAP {
            let mut start = self.last_output.len() - CAP;
            while !self.last_output.is_char_boundary(start) {
                start += 1;
            }
            self.last_output.drain(..start);
        }
    }

    fn drain_rx(&mut self) {
        if let Some(rx) = &self.rx {
            while let Ok(line) = rx.try_recv() {
                self.pending.push_back(line);
            }
        }
    }

    fn rx_empty(&mut self) -> bool {
        self.drain_rx();
        self.pending.is_empty()
    }

    fn reap(&mut self) {
        let code = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.code().unwrap_or(1)),
                _ => None,
            },
            None => None,
        };
        if let Some(code) = code {
            self.last_exit = Some(code);
            self.child = None;
            self.stdin = None;
            self.exited = true;
            clear_agent_pgid();
        }
    }

    fn alive(&mut self) -> bool {
        self.reap();
        self.child.is_some() && !self.exited
    }

    fn kill_child(&mut self, sig: i32) {
        if let Some(child) = self.child.as_ref() {
            signal_group(child.id(), sig);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.exited = true;
        self.rx = None;
        clear_agent_pgid();
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// EPIPE means the child exited without reading. That is a failed turn, not a
/// fatal error: drop the pipe and keep the child so its exit and stderr arrive.
fn write_ignoring_pipe(writer: &mut impl Write, bytes: &[u8]) -> Result<bool> {
    match writer.write_all(bytes) {
        Ok(()) => match writer.flush() {
            Ok(()) => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(true),
            Err(err) => Err(err).context("write prompt"),
        },
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(true),
        Err(err) => Err(err).context("write prompt"),
    }
}

fn spawn_readers(
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
) -> (Receiver<String>, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::channel();
    let readers = Arc::new(AtomicUsize::new(2));
    spawn_one(stdout, tx.clone(), Arc::clone(&readers));
    spawn_one(stderr, tx, Arc::clone(&readers));
    (rx, readers)
}

fn spawn_one(reader: impl Read + Send + 'static, tx: Sender<String>, readers: Arc<AtomicUsize>) {
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        loop {
            let mut buf = Vec::new();
            match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        readers.fetch_sub(1, Ordering::SeqCst);
    });
}

fn signal_group(pid: u32, sig: i32) {
    unsafe {
        kill(-(pid as i32), sig);
    }
}

static AGENT_PGID: AtomicI32 = AtomicI32::new(0);

pub fn track_agent_pgid(pid: u32) {
    AGENT_PGID.store(pid as i32, Ordering::SeqCst);
}

pub fn clear_agent_pgid() {
    AGENT_PGID.store(0, Ordering::SeqCst);
}

pub fn kill_tracked_agent() {
    let pgid = AGENT_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        signal_group(pgid as u32, 15);
    }
}

static FATAL_GUARDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TERMIOS_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct TermiosBytes(std::cell::UnsafeCell<[u8; 256]>);
unsafe impl Sync for TermiosBytes {}
static TERMIOS_BYTES: TermiosBytes = TermiosBytes(std::cell::UnsafeCell::new([0; 256]));

pub fn install_fatal_signals() {
    if FATAL_GUARDS.swap(true, Ordering::SeqCst) {
        return;
    }
    save_termios();
    unsafe {
        signal(1, on_fatal as usize);
        signal(15, on_fatal as usize);
    }
}

fn save_termios() {
    let size = std::mem::size_of::<libc::termios>();
    if size > 256 {
        return;
    }
    unsafe {
        let ptr = (*TERMIOS_BYTES.0.get()).as_mut_ptr() as *mut libc::termios;
        if libc::tcgetattr(libc::STDIN_FILENO, ptr) == 0 {
            TERMIOS_READY.store(true, Ordering::SeqCst);
        }
    }
}

/// Restore the terminal saved at startup. A missing snapshot does not call
/// `tcsetattr`. Used from the SIGTERM/SIGHUP handler; it does not take locks.
pub fn restore_termios_snapshot(saved: bool) -> bool {
    if !saved {
        return false;
    }
    unsafe {
        let ptr = (*TERMIOS_BYTES.0.get()).as_ptr() as *const libc::termios;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, ptr) == 0
    }
}

extern "C" fn on_fatal(sig: i32) {
    let pgid = AGENT_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        unsafe { kill(-pgid, 15) };
    }
    let _ = restore_termios_snapshot(TERMIOS_READY.load(Ordering::SeqCst));
    const SEQ: &[u8] = b"\x1b[?1049l\x1b[0m\x1b[?25h\n";
    unsafe {
        write(1, SEQ.as_ptr(), SEQ.len());
        _exit(128 + sig);
    }
}

const GH_GUARD: &str = r#"#!/bin/sh
# Accident guard only. The deny scan never rewrites argv; exec keeps "$@".
real="${GHOST_REAL_GH:-}"
if [ -z "$real" ] || [ ! -x "$real" ]; then
  echo "ghost: real gh is not available" >&2
  exit 127
fi
expect=""
pos=0
a1=""
a2=""
method=""
merge_api=0
for arg in "$@"; do
  if [ -n "$expect" ]; then
    if [ "$expect" = "method" ]; then
      up=$(printf '%s' "$arg" | tr '[:lower:]' '[:upper:]')
      if [ "$up" = "DELETE" ]; then
        method=DELETE
      fi
    fi
    case "$arg" in
      *[Mm][Ee][Rr][Gg][Ee][Pp][Uu][Ll][Ll][Rr][Ee][Qq][Uu][Ee][Ss][Tt]*) merge_api=1 ;;
    esac
    expect=""
    continue
  fi
  case "$arg" in
    -X|--method) expect=method ;;
    -XDELETE|-Xdelete|-XDelete|--method=DELETE|--method=delete|--method=Delete)
      method=DELETE ;;
    -R|--repo|--hostname|--jq|--template|--cache|-f|-F|--raw-field|--input|-H|--header|--field)
      expect=skip ;;
    --repo=*|--hostname=*|--jq=*|--field=*|--raw-field=*|--input=*|--header=*|--cache=*|--template=*|--method=*)
      case "$arg" in
        --method=DELETE|--method=delete|--method=Delete) method=DELETE ;;
        *[Mm][Ee][Rr][Gg][Ee][Pp][Uu][Ll][Ll][Rr][Ee][Qq][Uu][Ee][Ss][Tt]*) merge_api=1 ;;
      esac
      ;;
    -*) ;;
    *)
      pos=$((pos + 1))
      if [ "$pos" -eq 1 ]; then a1=$arg; fi
      if [ "$pos" -eq 2 ]; then a2=$arg; fi
      case "$arg" in
        *[Mm][Ee][Rr][Gg][Ee][Pp][Uu][Ll][Ll][Rr][Ee][Qq][Uu][Ee][Ss][Tt]*) merge_api=1 ;;
        */merge|*/merge/|*/merge\?*|*/merges|*/merges/|*/merges\?*) merge_api=1 ;;
      esac
      ;;
  esac
done
case "$a1 $a2" in
  "pr merge"|"repo delete"|"repo archive"|"issue delete"|"release delete")
    echo "ghost: refusing gh $a1 $a2" >&2
    exit 99
    ;;
esac
if [ "$a1" = "api" ]; then
  if [ "$method" = "DELETE" ]; then
    echo "ghost: refusing gh api DELETE" >&2
    exit 99
  fi
  if [ "$merge_api" -eq 1 ]; then
    echo "ghost: refusing gh api merge" >&2
    exit 99
  fi
fi
if [ "$a1" = "issue" ] && [ "$a2" = "create" ]; then
  errf="${TMPDIR:-/tmp}/ghost-issue-create-$$.err"
  out=$("$real" "$@" 2>"$errf")
  code=$?
  if [ -s "$errf" ]; then
    cat "$errf" >&2
  fi
  printf '%s\n' "$out"
  if [ "$code" -eq 0 ] && [ -n "${GHOST_CREATED_ISSUES:-}" ]; then
    blob=$(printf '%s\n' "$out"; cat "$errf" 2>/dev/null)
    num=$(printf '%s\n' "$blob" | sed -n 's#.*issues/\([0-9][0-9]*\).*#\1#p' | head -n 1)
    if [ -z "$num" ]; then
      num=$(printf '%s\n' "$blob" | sed -n 's/.*"number":[ ]*\([0-9][0-9]*\).*/\1/p' | head -n 1)
    fi
    if [ -n "$num" ]; then
      mkdir -p "$(dirname "$GHOST_CREATED_ISSUES")" 2>/dev/null
      printf '%s\n' "$num" >> "$GHOST_CREATED_ISSUES"
    fi
  fi
  rm -f "$errf"
  exit "$code"
fi
exec "$real" "$@"
"#;

const GIT_GUARD: &str = r#"#!/bin/sh
# Keeps a fork PR from updating origin's same-name branch. Not a security boundary.
# Global options are skipped before the subcommand, so `git -C . push` is still a push.
real="${GHOST_REAL_GIT:-}"
if [ -z "$real" ] || [ ! -x "$real" ]; then
  echo "ghost: real git is not available" >&2
  exit 127
fi
cmd=""
expect=""
for arg in "$@"; do
  if [ -n "$expect" ]; then
    expect=""
    continue
  fi
  case "$arg" in
    -C|-c|--git-dir|--work-tree|--namespace|--exec-path|--super-prefix|--config-env|--list-cmds)
      expect=arg
      ;;
    -C*|-c*|--git-dir=*|--work-tree=*|--namespace=*|--exec-path=*|--super-prefix=*|--config-env=*|--list-cmds=*)
      ;;
    --)
      ;;
    -P|--no-pager|--paginate|-p|--bare|--no-replace-objects|--literal-pathspecs|--glob-pathspecs|--noglob-pathspecs|--icase-pathspecs|--no-optional-locks|--no-advice|--version|--help|-v|-h)
      ;;
    -*)
      ;;
    *)
      cmd=$arg
      break
      ;;
  esac
done
if [ "$cmd" = "push" ]; then
  if [ "$GHOST_PUSH_MODE" = "readonly" ]; then
    echo "ghost: refusing git push${GHOST_PUSH_REASON:+: $GHOST_PUSH_REASON}" >&2
    exit 99
  fi
  if [ -n "$GHOST_PUSH_REMOTE" ] && [ -n "$GHOST_PUSH_REFSPEC" ]; then
    exec "$real" push "$GHOST_PUSH_REMOTE" "$GHOST_PUSH_REFSPEC"
  fi
fi
exec "$real" "$@"
"#;

pub fn install_git_guard(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("git");
    fs::write(&path, GIT_GUARD)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
    }
    Ok(dir.to_path_buf())
}

pub fn install_gh_guard(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("gh");
    fs::write(&path, GH_GUARD)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
    }
    Ok(dir.to_path_buf())
}

extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn signal(sig: i32, handler: usize) -> usize;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn _exit(code: i32) -> !;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigterm_skips_termios_when_nothing_was_saved() {
        assert!(!restore_termios_snapshot(false));
    }

    #[test]
    fn auto_pick_order_and_explicit() {
        let avail = |k: AgentKind| k == AgentKind::Grok;
        assert_eq!(pick_agent(None, avail).unwrap(), AgentKind::Grok);
        assert!(pick_agent(Some(AgentKind::Codex), avail).is_err());
        assert_eq!(
            pick_agent(Some(AgentKind::Grok), avail).unwrap(),
            AgentKind::Grok
        );
        assert!(pick_agent(None, |_| false).is_err());

        let order = [AgentKind::Codex, AgentKind::Claude, AgentKind::Grok];
        assert_eq!(
            pick_agent(None, |k| k == AgentKind::Claude || k == AgentKind::Grok).unwrap(),
            AgentKind::Claude
        );
        assert_eq!(order[0], AgentKind::Codex);
    }

    #[test]
    fn follow_up_always_resumes_the_same_id() {
        let id = "11111111-1111-4111-8111-111111111111";
        let first = plan_launch(AgentKind::Claude, None, id);
        assert_eq!(
            first,
            Launch::Start {
                preset_session_id: Some(id.into())
            }
        );
        let next = plan_launch(AgentKind::Claude, Some(id), "should-not-be-used");
        assert_eq!(
            next,
            Launch::Resume {
                session_id: id.into()
            }
        );
        // Issue mode transitioning to --code keeps the id and does not start over.
        let transition = plan_launch(AgentKind::Claude, Some(id), "other");
        assert_eq!(transition, next);

        let codex_first = plan_launch(AgentKind::Codex, None, id);
        assert_eq!(
            codex_first,
            Launch::Start {
                preset_session_id: None
            }
        );
        let thread = "thread-9";
        assert_eq!(
            plan_launch(AgentKind::Codex, Some(thread), id),
            Launch::Resume {
                session_id: thread.into()
            }
        );
    }

    #[test]
    fn command_lines_continue_one_session() {
        let dir = Path::new("/work/acme__app");
        let file = Path::new("/tmp/prompt.md");
        let id = "11111111-1111-4111-8111-111111111111";
        let start = command_for(
            AgentKind::Claude,
            &Launch::Start {
                preset_session_id: Some(id.into()),
            },
            dir,
            file,
        )
        .unwrap();
        assert!(start.args.contains(&"--session-id".into()));
        assert!(!start.args.iter().any(|a| a == "--resume"));
        assert!(!start.args.iter().any(|a| a == "--fork-session"));
        assert_eq!(start.delivery, Delivery::StreamJson);

        let resume = command_for(
            AgentKind::Claude,
            &Launch::Resume {
                session_id: id.into(),
            },
            dir,
            file,
        )
        .unwrap();
        assert!(resume.args.windows(2).any(|w| w == ["--resume", id]));
        assert!(!resume.args.iter().any(|a| a == "--session-id"));

        let codex = command_for(
            AgentKind::Codex,
            &Launch::Resume {
                session_id: "th-1".into(),
            },
            dir,
            file,
        )
        .unwrap();
        assert_eq!(
            codex.args,
            [
                "exec",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "resume",
                "th-1",
                "-",
            ]
        );
        assert!(!codex.args.iter().any(|a| a == "-C" || a == "--cd"));

        let grok = command_for(
            AgentKind::Grok,
            &Launch::Resume {
                session_id: id.into(),
            },
            dir,
            file,
        )
        .unwrap();
        assert!(grok.args.windows(2).any(|w| w == ["--resume", id]));
        assert!(!grok.args.iter().any(|a| a == "--session-id"));
        assert!(grok.args.iter().any(|a| a == "--yolo"));
        let batch = shell::prompt_env(PromptMode::Batch, None, None);
        assert!(batch
            .iter()
            .any(|(k, v)| *k == "GIT_TERMINAL_PROMPT" && v == "0"));
        assert!(batch
            .iter()
            .any(|(k, v)| { *k == "GIT_SSH_COMMAND" && v.contains("BatchMode=yes") }));

        let grok_new = command_for(
            AgentKind::Grok,
            &Launch::Start {
                preset_session_id: Some(id.into()),
            },
            dir,
            file,
        )
        .unwrap();
        assert!(grok_new.args.windows(2).any(|w| w == ["--session-id", id]));
        assert!(!grok_new.args.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn argv_for_initial_and_resume_invocations() {
        let dir = Path::new("/work/acme__app");
        let file = Path::new("/tmp/prompt.md");
        let id = "11111111-1111-4111-8111-111111111111";
        let args = |kind, launch: Launch| command_for(kind, &launch, dir, file).unwrap().args;

        assert_eq!(
            args(
                AgentKind::Codex,
                Launch::Start {
                    preset_session_id: None,
                }
            ),
            vec![
                "exec".to_string(),
                "--json".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--skip-git-repo-check".into(),
                "-C".into(),
                "/work/acme__app".into(),
                "-".into(),
            ]
        );
        let codex_resume = args(
            AgentKind::Codex,
            Launch::Resume {
                session_id: "01a16307-thread".into(),
            },
        );
        assert_eq!(
            codex_resume,
            vec![
                "exec".to_string(),
                "--json".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--skip-git-repo-check".into(),
                "resume".into(),
                "01a16307-thread".into(),
                "-".into(),
            ]
        );
        assert!(codex_resume
            .iter()
            .all(|a| a != "-C" && a != "--cd" && a != "-s" && a != "--sandbox"));
        assert!(!codex_resume.iter().any(|a| a.contains("acme__app")));

        assert_eq!(
            args(
                AgentKind::Claude,
                Launch::Start {
                    preset_session_id: Some(id.into()),
                }
            ),
            vec![
                "--print".to_string(),
                "--dangerously-skip-permissions".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--session-id".into(),
                id.into(),
            ]
        );
        let claude_resume = args(
            AgentKind::Claude,
            Launch::Resume {
                session_id: id.into(),
            },
        );
        assert_eq!(
            claude_resume,
            vec![
                "--print".to_string(),
                "--dangerously-skip-permissions".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--resume".into(),
                id.into(),
            ]
        );
        assert!(!claude_resume
            .iter()
            .any(|a| a == "--session-id" || a == "--fork-session"));

        assert_eq!(
            args(
                AgentKind::Grok,
                Launch::Start {
                    preset_session_id: Some(id.into()),
                }
            ),
            vec![
                "--prompt-file".to_string(),
                "/tmp/prompt.md".into(),
                "--session-id".into(),
                id.into(),
                "--output-format".into(),
                "streaming-json".into(),
                "--yolo".into(),
                "--cwd".into(),
                "/work/acme__app".into(),
            ]
        );
        let grok_resume = args(
            AgentKind::Grok,
            Launch::Resume {
                session_id: id.into(),
            },
        );
        assert_eq!(
            grok_resume,
            vec![
                "--prompt-file".to_string(),
                "/tmp/prompt.md".into(),
                "--resume".into(),
                id.into(),
                "--output-format".into(),
                "streaming-json".into(),
                "--yolo".into(),
                "--cwd".into(),
                "/work/acme__app".into(),
            ]
        );
        assert!(!grok_resume
            .iter()
            .any(|a| a == "--session-id" || a == "--fork-session"));
    }

    #[test]
    fn resume_argv_usage_failure_does_not_count_as_a_turn() {
        let resume = Launch::Resume {
            session_id: "01a16307".into(),
        };
        let usage = "error: unexpected argument '-C' found\n\nUsage: codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]\n";
        assert!(!counts_as_agent_turn(&resume, Some(2), usage));
        assert!(!counts_as_agent_turn(&resume, Some(2), ""));
        assert!(!counts_as_agent_turn(
            &resume,
            Some(1),
            "error: unexpected option '--sandbox' found\nUsage: codex exec resume"
        ));
        let failed = r#"{"type":"turn.failed"}"#;
        assert!(counts_as_agent_turn(&resume, Some(1), failed));
        assert!(counts_as_agent_turn(&resume, Some(0), ""));
        let missing = "Error: thread/resume: thread/resume failed: no rollout found for thread id ec326e07-1111-4111-8111-111111111111 (code -32600)\n";
        assert!(resume_unavailable(Some(1), missing));
        assert!(!counts_as_agent_turn(&resume, Some(1), missing));
        assert!(!resume_unavailable(Some(0), missing));
        assert!(!resume_unavailable(Some(1), failed));
        let start = Launch::Start {
            preset_session_id: None,
        };
        assert!(counts_as_agent_turn(&start, Some(2), usage));
        assert!(!record_progress(false, Some(2), false, false));
    }

    #[test]
    fn resume_spawn_uses_the_worktree_as_process_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let work = dir.join("worktree");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let stub = dir.join("stub-agent");
        std::fs::write(&stub, "#!/bin/sh\npwd -P\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        let home = dir.join("home");
        let mut session = AgentSession::new(AgentKind::Codex, work.clone(), &home, "main");
        session.set_stub(stub);
        session.seed(Some("01a16307".into()));
        let launch = session.dispatch("follow up").unwrap();
        assert!(matches!(launch, Launch::Resume { .. }));
        let start = std::time::Instant::now();
        while !session.turn_finished() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "stub hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = session.recent_output().trim().to_string();
        assert_eq!(output, work.display().to_string());
        session.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_session_events_and_hides_metadata() {
        let line = r#"{"type":"thread.started","thread_id":"th-42"}"#;
        assert_eq!(extract_session_id(line).as_deref(), Some("th-42"));
        assert!(present_line(line).is_empty());
        assert!(is_turn_end(
            AgentKind::Codex,
            r#"{"type":"turn.completed"}"#
        ));

        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"},{"type":"tool_use","name":"Bash","input":{"command":"gh pr view 3"}}]}}"#;
        let shown = present_line(assistant);
        assert_eq!(
            shown,
            vec!["hello".to_string(), "$ gh pr view 3".to_string()]
        );
        assert_eq!(
            extract_session_id(r#"{"type":"result","session_id":"abc","result":"done"}"#)
                .as_deref(),
            Some("abc")
        );
        assert!(present_line(r#"{"type":"result","result":"done"}"#).is_empty());
        assert_eq!(
            present_line("plain agent line"),
            vec!["plain agent line".to_string()]
        );
    }

    #[test]
    fn stub_agent_exit_1_does_not_record_and_resume_can_start_fresh() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-stub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("stub-agent");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho '{\"type\":\"turn.failed\"}'\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        let home = dir.join("home");
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut session = AgentSession::new(AgentKind::Codex, work, &home, "main");
        session.set_stub(stub);
        session.seed(Some("dead-session".into()));
        let launch = session
            .dispatch("same context as the failed review")
            .unwrap();
        assert!(matches!(launch, Launch::Resume { .. }));
        let start = std::time::Instant::now();
        while !session.turn_finished() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "stub hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(session.last_exit_code(), Some(1));
        assert!(!record_progress(
            false,
            session.last_exit_code(),
            session.clean_live(),
            session.saw_error()
        ));
        session.forget_session();
        let again = session
            .dispatch("same context as the failed review")
            .unwrap();
        assert!(
            matches!(again, Launch::Start { .. }),
            "resume failure starts a fresh session"
        );
        session.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_rollout_starts_a_fresh_session_and_runs_the_prompt() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-rollout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let prompt_out = dir.join("prompt.txt");
        let stub = dir.join("stub-codex");
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" resume \"*)\n    echo 'Error: thread/resume: thread/resume failed: no rollout found for thread id ec326e07-1111-4111-8111-111111111111 (code -32600)' >&2\n    exit 1\n    ;;\nesac\ncat > '{}'\necho '{{\"type\":\"turn.completed\"}}'\nexit 0\n",
            prompt_out.display()
        );
        std::fs::write(&stub, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        let home = dir.join("home");
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut session = AgentSession::new(AgentKind::Codex, work, &home, "main");
        session.set_stub(stub);
        session.seed(Some("ec326e07-1111-4111-8111-111111111111".into()));
        let prompt = "SITUATION CHECK please";
        let launch = session.dispatch(prompt).unwrap();
        assert!(
            matches!(launch, Launch::Resume { .. }),
            "a seeded id resumes"
        );
        let start = std::time::Instant::now();
        while !session.turn_finished() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "stub hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let output = session.recent_output().to_string();
        assert!(output.contains("no rollout found"), "stderr was {output}");
        assert!(!counts_as_agent_turn(
            &launch,
            session.last_exit_code(),
            &output
        ));
        assert!(resume_unavailable(session.last_exit_code(), &output));
        session.forget_session();
        let again = session.dispatch(prompt).unwrap();
        assert!(
            matches!(again, Launch::Start { .. }),
            "missing rollout starts fresh, got {again:?}"
        );
        let start = std::time::Instant::now();
        while !session.turn_finished() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "fresh stub hung"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(session.last_exit_code(), Some(0));
        assert!(counts_as_agent_turn(
            &again,
            session.last_exit_code(),
            session.recent_output()
        ));
        let saved = std::fs::read_to_string(&prompt_out).unwrap();
        assert!(
            saved.contains(prompt),
            "fresh session did not receive the prompt: {saved}"
        );
        session.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saw_error_blocks_progress_even_on_a_zero_exit() {
        assert!(!record_progress(false, Some(0), false, true));
        assert!(record_progress(false, Some(0), false, false));
        assert!(!record_progress(false, None, true, true));
        assert!(record_progress(false, None, true, false));
        assert!(!record_progress(true, Some(0), false, false));
    }

    #[test]
    fn gh_wrapper_passes_argv_through_and_blocks_accidental_merge() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-gh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = install_gh_guard(&dir).unwrap();
        let real = dir.join("real-gh");
        let argv_out = dir.join("argv");
        std::fs::write(
            &real,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$GHOST_ARGV_OUT\"\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&real).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&real, perms).unwrap();
        }
        let guard = bin.join("gh");
        let run = |args: &[&str]| {
            let _ = std::fs::remove_file(&argv_out);
            std::process::Command::new(&guard)
                .args(args)
                .env("GHOST_REAL_GH", &real)
                .env("GHOST_ARGV_OUT", &argv_out)
                .output()
                .unwrap()
        };
        let passed = |args: &[&str]| {
            let out = run(args);
            assert_eq!(
                out.status.code(),
                Some(0),
                "stderr {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let bytes = std::fs::read(&argv_out).unwrap();
            bytes
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8(s.to_vec()).unwrap())
                .collect::<Vec<_>>()
        };
        let comment = [
            "pr",
            "comment",
            "1",
            "--repo",
            "o/r",
            "--body",
            "hello world",
            "--body=say \"hi there\"",
            "--json",
            "title,body",
        ];
        assert_eq!(passed(&comment), comment);
        let create = [
            "pr",
            "create",
            "--repo",
            "o/r",
            "--title",
            "hello world",
            "--body",
            "line one\nline two",
            "-f",
            "body=hello world",
        ];
        assert_eq!(passed(&create), create);
        let reply = [
            "api",
            "--method",
            "POST",
            "repos/o/r/pulls/1/comments/9/replies",
            "-f",
            "body=hello world",
        ];
        assert_eq!(passed(&reply), reply);

        let refused = |args: &[&str], needle: &str| {
            let out = run(args);
            assert_eq!(out.status.code(), Some(99), "{args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(needle),
                "{args:?} stderr {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !argv_out.exists(),
                "denied command must not reach real gh: {args:?}"
            );
        };
        refused(&["pr", "merge", "2", "--squash"], "refusing");
        refused(&["repo", "delete", "acme/app", "--yes"], "refusing");
        refused(&["api", "-X", "DELETE", "repos/acme/app"], "DELETE");
        refused(&["api", "--method=DELETE", "repos/acme/app"], "DELETE");
        refused(&["api", "-XDELETE", "repos/acme/app"], "DELETE");
        refused(&["api", "repos/o/r/pulls/1/merge"], "merge");
        refused(&["api", "-X", "PUT", "repos/o/r/pulls/1/merge"], "merge");
        refused(
            &[
                "api",
                "graphql",
                "-f",
                "query=mutation { mergePullRequest(input:{}) { clientMutationId } }",
            ],
            "merge",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gh_wrapper_records_issue_create_and_not_a_later_user_issue() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-gh-create-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = install_gh_guard(&dir).unwrap();
        let real = dir.join("real-gh");
        let created = dir.join("nest").join("created-issues");
        std::fs::write(
            &real,
            "#!/bin/sh\nprintf '%s\\n' \"$GHOST_FAKE_OUT\"\nif [ -n \"$GHOST_FAKE_ERR\" ]; then printf '%s\\n' \"$GHOST_FAKE_ERR\" >&2; fi\nexit \"${GHOST_FAKE_CODE:-0}\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&real).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&real, perms).unwrap();
        }
        let guard = bin.join("gh");
        let run = |args: &[&str], out: &str, err: &str, code: &str| {
            std::process::Command::new(&guard)
                .args(args)
                .env("GHOST_REAL_GH", &real)
                .env("GHOST_CREATED_ISSUES", &created)
                .env("GHOST_FAKE_OUT", out)
                .env("GHOST_FAKE_ERR", err)
                .env("GHOST_FAKE_CODE", code)
                .output()
                .unwrap()
        };
        let created_url = run(
            &["issue", "create", "--title", "from the agent"],
            "https://github.com/acme/app/issues/44",
            "",
            "0",
        );
        assert_eq!(created_url.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&created_url.stdout).contains("issues/44"));
        assert_eq!(crate::state::read_created_issues(&created), vec![44]);

        run(
            &["issue", "create", "--json", "number"],
            r#"{"number":81}"#,
            "",
            "0",
        );
        assert_eq!(crate::state::read_created_issues(&created), vec![44, 81]);

        // A comment, or a pull request, can mention an issue URL. That is not `gh issue create`.
        run(
            &[
                "issue",
                "comment",
                "44",
                "--body",
                "see https://github.com/acme/app/issues/99",
            ],
            "https://github.com/acme/app/issues/99",
            "",
            "0",
        );
        run(
            &["pr", "create", "--title", "ship"],
            "https://github.com/acme/app/pull/7",
            "",
            "0",
        );
        run(
            &["issue", "create", "--title", "failed"],
            "https://github.com/acme/app/issues/100",
            "HTTP 502",
            "1",
        );
        assert_eq!(
            crate::state::read_created_issues(&created),
            vec![44, 81],
            "only a successful gh issue create is recorded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_files_are_per_process_and_per_slot() {
        let home = Path::new("/home/u/.ghost");
        let code = prompt_directory(home, "pr-4-code");
        let review = prompt_directory(home, "pr-4-review");
        assert_ne!(code, review);
        let pid = std::process::id();
        assert!(code.ends_with(format!("pid{pid}-pr-4-code")));
        assert!(!code.ends_with("prompts"));
        assert_ne!(
            prompt_directory(home, "issue-1"),
            prompt_directory(home, "pr-1-code")
        );
    }

    #[test]
    fn missing_workdir_spawns_in_ghost_work() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-cwd-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = dir.join("home");
        let missing = dir.join("no-such-slot");
        let stub = dir.join("stub-agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&stub, "#!/bin/sh\npwd -P\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
        }
        let mut session = AgentSession::new(AgentKind::Codex, missing.clone(), &home, "pr-2-code");
        session.set_stub(stub);
        assert!(!missing.exists());
        session.dispatch("setup failed").unwrap();
        let start = std::time::Instant::now();
        while !session.turn_finished() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(session.last_exit_code(), Some(0));
        let fallback = home.join("work").canonicalize().unwrap();
        assert_eq!(
            session.recent_output().trim(),
            fallback.display().to_string()
        );
        let shared = home.join("prompts").join("codex.md");
        assert!(!shared.exists(), "prompts must not be a shared file");
        assert!(prompt_directory(&home, "pr-2-code")
            .join("codex.md")
            .is_file());
        session.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_exe(path: &Path, body: &str) {
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
    fn git_guard_skips_global_options_and_readonly_never_pushes() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-git-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = dir.join("pushed");
        let real = dir.join("real-git");
        write_exe(
            &real,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n",
                marker.display()
            ),
        );
        install_git_guard(&bin).unwrap();
        let guard = bin.join("git");
        let run = |args: &[&str], mode: &str, remote: &str, refspec: &str| {
            let _ = std::fs::remove_file(&marker);
            std::process::Command::new(&guard)
                .args(args)
                .env("GHOST_REAL_GIT", &real)
                .env("GHOST_PUSH_MODE", mode)
                .env("GHOST_PUSH_REASON", "fork head main is not pushable")
                .env("GHOST_PUSH_REMOTE", remote)
                .env("GHOST_PUSH_REFSPEC", refspec)
                .output()
                .unwrap()
        };

        let bypasses = [
            vec!["-C", ".", "push", "origin", "HEAD:main"],
            vec!["-C.", "push", "origin", "HEAD:main"],
            vec!["--git-dir", "/tmp/x", "push", "origin", "HEAD:main"],
            vec!["--git-dir=/tmp/x", "push", "origin", "HEAD:main"],
            vec!["--work-tree", "/tmp/y", "push", "origin", "HEAD:main"],
            vec![
                "--work-tree=/tmp/y",
                "-c",
                "user.name=t",
                "push",
                "origin",
                "HEAD:main",
            ],
            vec![
                "-c",
                "user.name=t",
                "-P",
                "--bare",
                "--namespace",
                "ns",
                "push",
                "origin",
                "HEAD:main",
            ],
            vec!["--no-pager", "--", "push", "origin", "HEAD:main"],
        ];
        for args in &bypasses {
            let out = run(args, "readonly", "", "");
            assert_eq!(
                out.status.code(),
                Some(99),
                "{args:?} stderr {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("refusing git push"),
                "{args:?}"
            );
            assert!(
                !marker.exists(),
                "readonly must not exec real git for {args:?}; removing the readonly check would push"
            );
        }

        let out = run(
            &["-C", ".", "push", "origin", "HEAD:main"],
            "push",
            "git@github.com:forker/app.git",
            "HEAD:feature",
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let recorded = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            recorded,
            "push\ngit@github.com:forker/app.git\nHEAD:feature\n"
        );

        let out = run(&["-C", ".", "status"], "readonly", "", "");
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "-C\n.\nstatus\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
