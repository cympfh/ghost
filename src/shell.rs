use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone)]
pub struct CmdOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn which(bin: &str, path_env: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let path = PathBuf::from(bin);
        return is_exec(&path).then_some(path);
    }
    path_env.split(':').find_map(|dir| {
        if dir.is_empty() {
            return None;
        }
        let path = Path::new(dir).join(bin);
        is_exec(&path).then_some(path)
    })
}

fn is_exec(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `Tty` inherits the terminal so ssh can ask for a passphrase.
/// `Batch` refuses prompts (`GIT_TERMINAL_PROMPT=0` and ssh `BatchMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Tty,
    Batch,
}

pub const CREDENTIAL_HINT: &str = "\
Unlock the key with `ssh-add`, or use HTTPS: `gh config set git_protocol https`.";

static SSH_KEY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Remember `--ssh-key` / `GHOST_SSH_KEY` for every later git and agent process.
pub fn set_ssh_key(key: Option<String>) {
    let key = key.filter(|s| !s.trim().is_empty());
    *SSH_KEY.lock().expect("ssh key") = key;
}

pub fn ssh_key() -> Option<String> {
    SSH_KEY.lock().expect("ssh key").clone()
}

fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@'))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// `None` means "do not set GIT_SSH_COMMAND" so ssh uses the user's config and agent.
/// A key sets `ssh -i PATH -o IdentitiesOnly=yes` and never a built-in default identity.
pub fn git_ssh_command(
    mode: PromptMode,
    key: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    if let Some(key) = key.map(str::trim).filter(|s| !s.is_empty()) {
        let mut cmd = format!("ssh -i {} -o IdentitiesOnly=yes", shell_quote(key));
        if mode == PromptMode::Batch {
            cmd.push_str(" -o BatchMode=yes");
        }
        return Some(cmd);
    }
    match mode {
        PromptMode::Tty => None,
        PromptMode::Batch => Some(batch_ssh_command(existing)),
    }
}

/// BatchMode only. Does not add `-i` or any key path.
pub fn batch_ssh_command(existing: Option<&str>) -> String {
    let Some(existing) = existing.map(str::trim).filter(|s| !s.is_empty()) else {
        return "ssh -o BatchMode=yes".into();
    };
    if existing.to_ascii_lowercase().contains("batchmode") {
        existing.to_string()
    } else {
        format!("{existing} -o BatchMode=yes")
    }
}

pub fn prompt_env(
    mode: PromptMode,
    ssh_key: Option<&str>,
    existing_ssh: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("GH_PROMPT_DISABLED", "1".into()),
        ("GH_NO_UPDATE_NOTIFIER", "1".into()),
    ];
    if mode == PromptMode::Batch {
        env.push(("GIT_TERMINAL_PROMPT", "0".into()));
    }
    if let Some(cmd) = git_ssh_command(mode, ssh_key, existing_ssh) {
        env.push(("GIT_SSH_COMMAND", cmd));
    }
    env
}

pub fn credential_hint(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "passphrase",
        "permission denied",
        "publickey",
        "could not read from remote",
        "terminal prompts disabled",
        "batchmode",
        "host key verification failed",
        "authentication failed",
        "correct access rights",
    ];
    if NEEDLES.iter().any(|needle| lower.contains(needle)) {
        Some(CREDENTIAL_HINT)
    } else {
        None
    }
}

pub fn with_credential_hint(text: &str) -> String {
    match credential_hint(text) {
        Some(hint) if !text.contains("ssh-add") => format!("{text}\n{hint}"),
        _ => text.to_string(),
    }
}

fn apply_prompt_env(cmd: &mut Command, mode: PromptMode) {
    let existing = std::env::var("GIT_SSH_COMMAND").ok();
    let key = ssh_key();
    for (name, value) in prompt_env(mode, key.as_deref(), existing.as_deref()) {
        cmd.env(name, value);
    }
    if mode == PromptMode::Tty {
        cmd.env_remove("GIT_TERMINAL_PROMPT");
    }
}

pub fn run(
    program: &str,
    args: &[String],
    dir: Option<&Path>,
    stdin_data: Option<&[u8]>,
) -> Result<CmdOut> {
    run_mode(program, args, dir, stdin_data, PromptMode::Batch)
}

pub fn run_tty(program: &str, args: &[String], dir: Option<&Path>) -> Result<CmdOut> {
    run_mode(program, args, dir, None, PromptMode::Tty)
}

fn run_mode(
    program: &str,
    args: &[String],
    dir: Option<&Path>,
    stdin_data: Option<&[u8]>,
    mode: PromptMode,
) -> Result<CmdOut> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    if which(program, &path_env).is_none() {
        anyhow::bail!("`{program}` is not on PATH");
    }
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    apply_prompt_env(&mut cmd, mode);
    match mode {
        PromptMode::Tty => {
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let status = cmd.status().with_context(|| format!("spawn {program}"))?;
            Ok(CmdOut {
                code: status.code().unwrap_or(1),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        PromptMode::Batch => capture_child(&mut cmd, stdin_data, &format!("spawn {program}")),
    }
}

pub fn cmd_timeout() -> Duration {
    std::env::var("GHOST_CMD_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120))
}

pub fn is_rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("http 429") || lower.contains("secondary rate")
}

fn capture_child(cmd: &mut Command, stdin_data: Option<&[u8]>, what: &str) -> Result<CmdOut> {
    let mut delay = Duration::from_secs(2);
    let mut last = None;
    for attempt in 0..4 {
        let out = capture_once(cmd, stdin_data, what, cmd_timeout())?;
        if attempt < 3 && is_rate_limited(&format!("{}\n{}", out.stdout, out.stderr)) {
            thread::sleep(delay);
            delay = delay.saturating_mul(2);
            last = Some(out);
            continue;
        }
        return Ok(out);
    }
    Ok(last.unwrap_or(CmdOut {
        code: 1,
        stdout: String::new(),
        stderr: "rate limit".into(),
    }))
}

fn capture_once(
    cmd: &mut Command,
    stdin_data: Option<&[u8]>,
    what: &str,
    timeout: Duration,
) -> Result<CmdOut> {
    unsafe {
        cmd.pre_exec(|| {
            if setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| what.to_string())?;
    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(data).ok();
        }
    }
    let mut stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).ok();
        buf
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).ok();
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() >= timeout => {
                let pid = child.id();
                unsafe {
                    kill(-(pid as i32), 9);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(err).context("wait for command"),
        }
    };
    let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).into_owned();
    match status {
        Some(status) => Ok(CmdOut {
            code: status.code().unwrap_or(1),
            stdout,
            stderr,
        }),
        None => anyhow::bail!("timed out after {}s\n{stderr}", timeout.as_secs()),
    }
}

extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

pub fn gh(args: &[String], dir: Option<&Path>, repo: Option<&str>) -> Result<CmdOut> {
    gh_mode(args, dir, repo, PromptMode::Batch)
}

pub fn gh_tty(args: &[String], dir: Option<&Path>, repo: Option<&str>) -> Result<CmdOut> {
    gh_mode(args, dir, repo, PromptMode::Tty)
}

fn gh_mode(
    args: &[String],
    dir: Option<&Path>,
    repo: Option<&str>,
    mode: PromptMode,
) -> Result<CmdOut> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    if which("gh", &path_env).is_none() {
        anyhow::bail!("`gh` is not on PATH");
    }
    let mut cmd = Command::new("gh");
    cmd.args(args);
    apply_prompt_env(&mut cmd, mode);
    if let Some(repo) = repo {
        cmd.env("GH_REPO", repo);
    }
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    match mode {
        PromptMode::Tty => {
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let status = cmd.status().context("run gh")?;
            Ok(CmdOut {
                code: status.code().unwrap_or(1),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        PromptMode::Batch => capture_child(&mut cmd, None, "run gh"),
    }
}

pub fn git(dir: &Path, args: &[String]) -> Result<CmdOut> {
    run("git", args, Some(dir), None).map_err(|e| anyhow!(e))
}

pub fn git_tty(dir: &Path, args: &[String]) -> Result<CmdOut> {
    run_tty("git", args, Some(dir)).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_an_executable() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-which-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-gh");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
        }
        assert!(which("fake-gh", dir.to_str().unwrap()).is_some());
        assert!(which("missing-ghost-bin", dir.to_str().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_env_refuses_prompts_and_tty_allows_them() {
        let batch = prompt_env(PromptMode::Batch, None, None);
        assert!(batch
            .iter()
            .any(|(k, v)| *k == "GIT_TERMINAL_PROMPT" && v == "0"));
        let ssh = batch.iter().find(|(k, _)| *k == "GIT_SSH_COMMAND").unwrap();
        assert!(ssh.1.contains("BatchMode=yes"));
        assert!(!ssh.1.contains("id_rsa"), "{}", ssh.1);
        assert!(!ssh.1.contains("-i "), "{}", ssh.1);
        let tty = prompt_env(PromptMode::Tty, None, None);
        assert!(tty
            .iter()
            .all(|(k, _)| { *k != "GIT_TERMINAL_PROMPT" && *k != "GIT_SSH_COMMAND" }));
    }

    #[test]
    fn ssh_key_sets_identities_only_and_does_not_default_to_id_rsa() {
        let batch = git_ssh_command(PromptMode::Batch, Some("/keys/id_ed25519"), None).unwrap();
        assert_eq!(
            batch,
            "ssh -i /keys/id_ed25519 -o IdentitiesOnly=yes -o BatchMode=yes"
        );
        let tty = git_ssh_command(PromptMode::Tty, Some("/keys/id_ed25519"), None).unwrap();
        assert_eq!(tty, "ssh -i /keys/id_ed25519 -o IdentitiesOnly=yes");
        assert!(git_ssh_command(PromptMode::Tty, None, None).is_none());
        let quoted = git_ssh_command(PromptMode::Tty, Some("/tmp/my key"), None).unwrap();
        assert!(quoted.contains("-i '/tmp/my key'"));
        assert!(quoted.contains("IdentitiesOnly=yes"));
        assert!(!quoted.contains("id_rsa"));
    }

    #[test]
    fn batch_ssh_keeps_an_existing_command() {
        let wrapped = batch_ssh_command(Some("ssh -i /keys/id_ed25519"));
        assert!(wrapped.starts_with("ssh -i /keys/id_ed25519"));
        assert!(wrapped.contains("BatchMode=yes"));
        assert_eq!(
            batch_ssh_command(Some("ssh -o BatchMode=yes -o IdentitiesOnly=yes")),
            "ssh -o BatchMode=yes -o IdentitiesOnly=yes"
        );
    }

    #[test]
    fn rate_limit_text_is_detected() {
        assert!(is_rate_limited("API rate limit exceeded"));
        assert!(is_rate_limited("HTTP 429"));
        assert!(!is_rate_limited("pull request not found"));
    }

    #[test]
    fn batch_command_times_out() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("sleepy");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
        }
        let mut cmd = Command::new(&bin);
        let err = capture_once(&mut cmd, None, "sleepy", Duration::from_millis(300)).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("timed out"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passphrase_failure_suggests_ssh_add_or_https() {
        let msg = with_credential_hint("Enter passphrase for key '/home/u/.ssh/id_ed25519':");
        assert!(msg.contains("ssh-add"));
        assert!(msg.contains("gh config set git_protocol https"));
        let denied = with_credential_hint("Permission denied (publickey).");
        assert!(denied.contains("ssh-add"));
        assert_eq!(
            with_credential_hint("fatal: not a git repository"),
            "fatal: not a git repository"
        );
    }
}
