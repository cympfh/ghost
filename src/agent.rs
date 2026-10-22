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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::Value;

use crate::shell::which;
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
/// A non-empty existing id always resumes, including across the issue → --code
/// transition and across ghost restarts.
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
        (AgentKind::Codex, Launch::Resume { session_id }) => AgentCommand {
            program: "codex",
            args: vec![
                "exec".into(),
                "resume".into(),
                session_id.clone(),
                "--json".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--skip-git-repo-check".into(),
                "-C".into(),
                dir,
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
            .map(|s| split_keep(s))
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

/// The long-lived handle ghost keeps from the first task until exit.
pub struct AgentSession {
    kind: AgentKind,
    workdir: PathBuf,
    prompt_dir: PathBuf,
    session_id: Option<String>,
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    rx: Option<Receiver<String>>,
    readers: Arc<AtomicUsize>,
    pending: VecDeque<String>,
    turn_open: bool,
    exited: bool,
}

impl AgentSession {
    pub fn new(kind: AgentKind, workdir: PathBuf, ghost_home: &Path) -> Self {
        Self {
            kind,
            workdir,
            prompt_dir: ghost_home.join("prompts"),
            session_id: None,
            child: None,
            stdin: None,
            rx: None,
            readers: Arc::new(AtomicUsize::new(0)),
            pending: VecDeque::new(),
            turn_open: false,
            exited: false,
        }
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
            if let Some(id) = extract_session_id(&raw) {
                self.session_id = Some(id);
            }
            if is_turn_end(self.kind, &raw) {
                self.turn_open = false;
            }
            shown.extend(present_line(&raw));
        }
        shown
    }

    pub fn turn_finished(&mut self) -> bool {
        let _ = self.poll_lines();
        if !self.turn_open {
            return true;
        }
        self.reap();
        if self.exited && self.readers.load(Ordering::SeqCst) == 0 && self.rx_empty() {
            self.turn_open = false;
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
        stdin
            .write_all(claude_user_line(prompt).as_bytes())
            .context("write claude prompt")?;
        stdin.flush().ok();
        Ok(())
    }

    fn spawn(&mut self, launch: &Launch, prompt: &str) -> Result<()> {
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
        let mut cmd = Command::new(spec.program);
        cmd.args(&spec.args)
            .current_dir(&self.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_NO_UPDATE_NOTIFIER", "1");
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
        let mut stdin = child.stdin.take().context("stdin")?;
        match spec.delivery {
            Delivery::StreamJson => {
                stdin
                    .write_all(claude_user_line(prompt).as_bytes())
                    .context("write claude prompt")?;
                stdin.flush().ok();
                self.stdin = Some(stdin);
            }
            Delivery::StdinClose => {
                stdin.write_all(prompt.as_bytes()).context("write prompt")?;
                if !prompt.ends_with('\n') {
                    stdin.write_all(b"\n").ok();
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
        let done = match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            None => false,
        };
        if done {
            self.child = None;
            self.stdin = None;
            self.exited = true;
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
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.stop();
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

extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(&codex.args[..3], ["exec", "resume", "th-1"]);
        assert!(codex.args.iter().any(|a| a == "--json"));

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
}
