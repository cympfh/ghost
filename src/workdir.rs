use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::shell::{self, CmdOut, PromptMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreePlan {
    Clone,
    Reuse,
    Reclone { reason: String },
}

pub fn ghost_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("GHOST_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".ghost"))
}

pub fn work_path(home: &Path, owner: &str, repo: &str) -> PathBuf {
    home.join("work").join(format!("{owner}__{repo}"))
}

/// `<type>/<number>-<kebab>` with no tool prefix. ASCII-less titles become `<type>/<number>`.
pub fn issue_branch(number: u64, title: &str, body: &str) -> String {
    let kind = branch_type(title, body);
    let slug = kebab_slug(title);
    if slug.is_empty() {
        format!("{kind}/{number}")
    } else {
        format!("{kind}/{number}-{slug}")
    }
}

fn branch_type(title: &str, body: &str) -> &'static str {
    let text = format!("{title}\n{body}");
    if has_kw(&text, &["test", "tests", "testing"], &["テスト"]) {
        "test"
    } else if has_kw(&text, &["docs", "doc", "readme"], &["ドキュメント", "文書"]) {
        "docs"
    } else if has_kw(&text, &["refactor", "cleanup"], &["リファクタ"]) {
        "refactor"
    } else if has_kw(&text, &["chore", "deps", "ci"], &["依存"]) {
        "chore"
    } else if has_kw(
        &text,
        &[
            "fix", "fixes", "fixed", "bug", "bugs", "crash", "crashes", "error", "errors", "fail",
            "fails", "failed", "failing", "failure", "broken",
        ],
        &["バグ", "修正", "クラッシュ", "落ちる"],
    ) {
        "fix"
    } else {
        "feat"
    }
}

fn has_kw(text: &str, ascii: &[&str], extra: &[&str]) -> bool {
    let lower = text.to_lowercase();
    if extra.iter().any(|kw| lower.contains(kw)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|word| ascii.iter().any(|kw| word == *kw))
}

fn kebab_slug(title: &str) -> String {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    let mut out = String::new();
    for token in tokens {
        let next = if out.is_empty() {
            token.len()
        } else {
            out.len() + 1 + token.len()
        };
        if next > 40 {
            if out.is_empty() {
                return token.chars().take(40).collect();
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&token);
    }
    out
}

pub fn slot_path(home: &Path, owner: &str, repo: &str, slot: &str) -> PathBuf {
    home.join("work")
        .join(format!("{owner}__{repo}.wt"))
        .join(slot)
}

pub fn origin_matches(url: &str, owner: &str, repo: &str) -> bool {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let url_l = trimmed.to_ascii_lowercase();
    let owner_l = owner.to_ascii_lowercase();
    let repo_l = repo.to_ascii_lowercase();
    let slash = format!("/{owner_l}/{repo_l}");
    let scp = format!(":{owner_l}/{repo_l}");
    url_l.ends_with(&slash) || url_l.ends_with(&scp)
}

pub fn plan_worktree(
    exists: bool,
    git_ok: bool,
    origin: Option<&str>,
    owner: &str,
    repo: &str,
) -> WorktreePlan {
    if !exists {
        return WorktreePlan::Clone;
    }
    if !git_ok {
        return WorktreePlan::Reclone {
            reason: "existing directory is not a usable git checkout".into(),
        };
    }
    match origin {
        Some(url) if origin_matches(url, owner, repo) => WorktreePlan::Reuse,
        Some(url) => WorktreePlan::Reclone {
            reason: format!("origin is {url}, not {owner}/{repo}"),
        },
        None => WorktreePlan::Reclone {
            reason: "no origin remote".into(),
        },
    }
}

pub fn prepare(home: &Path, owner: &str, repo: &str) -> Result<(PathBuf, String)> {
    let path = work_path(home, owner, repo);
    let exists = path.exists();
    let checkout_ok = exists && git_ok(&path);
    let origin = if checkout_ok { git_origin(&path) } else { None };
    let plan = plan_worktree(exists, checkout_ok, origin.as_deref(), owner, repo);
    let slug = format!("{owner}/{repo}");
    match plan {
        WorktreePlan::Reuse => {
            match git_net(
                &path,
                &["fetch".into(), "origin".into(), "--prune".into()],
                PromptMode::Tty,
            ) {
                Ok(out) if out.code == 0 => Ok((path, "reused clone".into())),
                Ok(out) if git_ok(&path) => Ok((
                    path,
                    format!("reused clone (fetch failed: {})", explain(&out)),
                )),
                Ok(_) | Err(_) => {
                    let reason = "fetch failed and the checkout is unusable";
                    reclone(&path, &slug, reason)
                }
            }
        }
        WorktreePlan::Clone => clone_into(&path, &slug, "cloned"),
        WorktreePlan::Reclone { reason } => reclone(&path, &slug, &reason),
    }
}

fn reclone(path: &Path, slug: &str, reason: &str) -> Result<(PathBuf, String)> {
    if path.exists() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into());
        let aside = path.with_file_name(format!("{name}.broken-{stamp}"));
        fs::rename(path, &aside)
            .with_context(|| format!("move broken checkout {}", path.display()))?;
    }
    let (path, _) = clone_into(path, slug, "recloned")?;
    Ok((path, format!("recloned ({reason})")))
}

fn clone_into(path: &Path, slug: &str, verb: &str) -> Result<(PathBuf, String)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    }
    let out = shell::gh_tty(
        &[
            "repo".into(),
            "clone".into(),
            slug.to_string(),
            path.display().to_string(),
        ],
        None,
        Some(slug),
    )?;
    if out.code != 0 {
        anyhow::bail!("gh repo clone {slug} failed: {}", explain(&out));
    }
    Ok((path.to_path_buf(), verb.to_string()))
}

#[derive(Debug, Clone)]
pub enum Checkout {
    Pr(u64),
    Issue { branch: String, base: String },
}

pub fn sync_and_checkout(
    path: &Path,
    slug: &str,
    checkout: Checkout,
    mode: PromptMode,
) -> Result<String> {
    let fetch = git_net(
        path,
        &["fetch".into(), "origin".into(), "--prune".into()],
        mode,
    )?;
    let mut note = String::new();
    if fetch.code != 0 {
        let msg = format!("git fetch failed: {}", explain(&fetch));
        if !git_ok(path) {
            anyhow::bail!(msg);
        }
        // The repo is still usable. Do not sit on a hidden passphrase prompt.
        note = msg;
    }
    let status = git(path, &["status".into(), "--porcelain".into()])?;
    if status.code == 0 && !status.stdout.trim().is_empty() {
        let stash = git(
            path,
            &[
                "stash".into(),
                "push".into(),
                "--include-untracked".into(),
                "-m".into(),
                "autostash".into(),
            ],
        )?;
        if stash.code != 0 {
            anyhow::bail!(
                "working tree is dirty and stash failed: {}",
                one_line(&stash.stderr, &stash.stdout)
            );
        }
        note = "stashed local changes".into();
    }
    match checkout {
        Checkout::Pr(number) => {
            let out = gh_net(
                &[
                    "pr".into(),
                    "checkout".into(),
                    number.to_string(),
                    "--repo".into(),
                    slug.to_string(),
                ],
                Some(path),
                Some(slug),
                mode,
            )?;
            if out.code != 0 {
                let msg = format!("gh pr checkout {number} failed: {}", explain(&out));
                if shell::credential_hint(&msg).is_some() && git_ok(path) {
                    note = join_note(note, msg);
                } else {
                    anyhow::bail!(msg);
                }
            }
        }
        Checkout::Issue { branch, base } => {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            let has_remote = git(
                path,
                &[
                    "show-ref".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    remote_ref,
                ],
            )
            .map(|o| o.code == 0)
            .unwrap_or(false);
            let start = if has_remote {
                format!("origin/{branch}")
            } else {
                format!("origin/{base}")
            };
            let out = git(path, &["checkout".into(), "-B".into(), branch, start])?;
            if out.code != 0 {
                anyhow::bail!("git checkout failed: {}", explain(&out));
            }
        }
    }
    Ok(note)
}

fn join_note(note: String, extra: String) -> String {
    if note.is_empty() {
        extra
    } else {
        format!("{note} · {extra}")
    }
}

pub fn has_commit(path: &Path, sha: &str) -> bool {
    let spec = format!("{sha}^{{commit}}");
    git(path, &["cat-file".into(), "-e".into(), spec])
        .map(|o| o.code == 0)
        .unwrap_or(false)
}

pub fn diff_range(path: &Path, old: &str, new: &str) -> Result<String> {
    let out = git(path, &["diff".into(), old.into(), new.into()])?;
    if out.code != 0 {
        anyhow::bail!("git diff failed: {}", explain(&out));
    }
    Ok(out.stdout)
}

fn git_ok(path: &Path) -> bool {
    path.join(".git").exists()
        && git(path, &["rev-parse".into(), "--is-inside-work-tree".into()])
            .map(|o| o.code == 0 && o.stdout.trim() == "true")
            .unwrap_or(false)
}

fn git_origin(path: &Path) -> Option<String> {
    let out = git(path, &["remote".into(), "get-url".into(), "origin".into()]).ok()?;
    if out.code != 0 {
        return None;
    }
    let url = out.stdout.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

fn git(path: &Path, args: &[String]) -> Result<CmdOut> {
    shell::git(path, args).map_err(|e| anyhow!(e))
}

fn git_net(path: &Path, args: &[String], mode: PromptMode) -> Result<CmdOut> {
    match mode {
        PromptMode::Tty => shell::git_tty(path, args),
        PromptMode::Batch => shell::git(path, args),
    }
}

fn gh_net(
    args: &[String],
    dir: Option<&Path>,
    repo: Option<&str>,
    mode: PromptMode,
) -> Result<CmdOut> {
    match mode {
        PromptMode::Tty => shell::gh_tty(args, dir, repo),
        PromptMode::Batch => shell::gh(args, dir, repo),
    }
}

fn explain(out: &CmdOut) -> String {
    if out.stdout.trim().is_empty() && out.stderr.trim().is_empty() {
        return "see the terminal output above".into();
    }
    shell::with_credential_hint(&one_line(&out.stderr, &out.stdout))
}

fn one_line(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unknown error")
        .trim()
        .to_string()
}

pub struct SlotLock {
    file: File,
}

pub fn lock_slot(dest: &Path) -> Result<SlotLock> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "slot".into());
    let lock_path = dest.with_file_name(format!("{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another ghost process is using {} ({})",
            dest.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(SlotLock { file })
}

impl Drop for SlotLock {
    fn drop(&mut self) {
        unsafe {
            flock(self.file.as_raw_fd(), LOCK_UN);
        }
    }
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

extern "C" {
    fn flock(fd: i32, op: i32) -> i32;
}

/// Add `dest` as a detached worktree of `main` so two PRs do not share a checkout.
pub fn ensure_worktree(main: &Path, dest: &Path) -> Result<()> {
    if dest.join(".git").exists() {
        return Ok(());
    }
    if dest.exists() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "wt".into());
        let aside = dest.with_file_name(format!("{name}.broken-{stamp}"));
        fs::rename(dest, &aside).with_context(|| format!("move {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let out = git(
        main,
        &[
            "worktree".into(),
            "add".into(),
            "--detach".into(),
            dest.display().to_string(),
            "HEAD".into(),
        ],
    )?;
    if out.code != 0 {
        anyhow::bail!("git worktree add failed: {}", explain(&out));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_layout_and_origin_match() {
        let home = Path::new("/home/u/.ghost");
        assert_eq!(
            work_path(home, "Cympfh", "Ghost"),
            PathBuf::from("/home/u/.ghost/work/Cympfh__Ghost")
        );
        assert_eq!(
            issue_branch(12, "Crash on empty config", ""),
            "fix/12-crash-on-empty-config"
        );
        assert_eq!(issue_branch(12, "クラッシュする", ""), "fix/12");
        assert_eq!(issue_branch(3, "Add a picker", ""), "feat/3-add-a-picker");
        assert_eq!(
            issue_branch(1, "Add fixture loader", ""),
            "feat/1-add-fixture-loader"
        );
        assert!(issue_branch(9, &"word ".repeat(30), "").len() <= "feat/9-".len() + 40);
        assert!(origin_matches(
            "https://github.com/Cympfh/Ghost.git",
            "cympfh",
            "Ghost"
        ));
        assert!(origin_matches(
            "git@github.com:cympfh/ghost.git",
            "Cympfh",
            "ghost"
        ));
        assert!(!origin_matches(
            "https://github.com/other/ghost.git",
            "cympfh",
            "ghost"
        ));
        assert!(!origin_matches(
            "https://github.com/cympfh/ghost-extra",
            "cympfh",
            "ghost"
        ));
    }

    #[test]
    fn plans_clone_reuse_and_reclone() {
        assert_eq!(
            plan_worktree(false, false, None, "a", "b"),
            WorktreePlan::Clone
        );
        assert_eq!(
            plan_worktree(true, true, Some("https://github.com/a/b"), "a", "b"),
            WorktreePlan::Reuse
        );
        assert!(matches!(
            plan_worktree(true, false, None, "a", "b"),
            WorktreePlan::Reclone { .. }
        ));
        assert!(matches!(
            plan_worktree(true, true, Some("https://github.com/x/y"), "a", "b"),
            WorktreePlan::Reclone { .. }
        ));
        assert!(matches!(
            plan_worktree(true, true, None, "a", "b"),
            WorktreePlan::Reclone { .. }
        ));
    }

    #[test]
    fn worktrees_are_per_slot_and_locked() {
        let root = std::env::temp_dir().join(format!(
            "ghost-wt-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = root.join("repo");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        git(&["init"]);
        fs::write(main.join("f"), "x\n").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-m", "init"]);
        let a = root.join("wt").join("pr-1");
        let b = root.join("wt").join("pr-2");
        ensure_worktree(&main, &a).unwrap();
        ensure_worktree(&main, &b).unwrap();
        assert!(a.join(".git").exists());
        assert!(b.join(".git").exists());
        assert_ne!(a, b);
        let held = lock_slot(&a).unwrap();
        assert!(lock_slot(&a).is_err());
        drop(held);
        assert!(lock_slot(&a).is_ok());
        let _ = fs::remove_dir_all(&root);
    }
}
