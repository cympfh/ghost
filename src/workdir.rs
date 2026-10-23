use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::setup::{SetupFailure, SetupStep};
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
/// The type comes from the title and labels. The issue body is not consulted:
/// a mention of "docs" in the body must not turn a bug fix into `docs/...`.
pub fn issue_branch(number: u64, title: &str, labels: &[&str]) -> String {
    let kind = branch_type(title, labels);
    let slug = kebab_slug(title);
    if slug.is_empty() {
        format!("{kind}/{number}")
    } else {
        format!("{kind}/{number}-{slug}")
    }
}

fn branch_type(title: &str, labels: &[&str]) -> &'static str {
    let mut text = title.to_string();
    for label in labels {
        text.push('\n');
        text.push_str(label);
    }
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

#[derive(Debug, Clone)]
pub struct PrepareOutcome {
    pub path: PathBuf,
    pub note: String,
    pub failures: Vec<SetupFailure>,
}

/// Clone or reuse the main checkout. Command failures are recorded and the
/// best directory still on disk is returned. The process does not stop here.
pub fn prepare(home: &Path, owner: &str, repo: &str) -> PrepareOutcome {
    let path = work_path(home, owner, repo);
    let exists = path.exists();
    let checkout_ok = exists && git_ok(&path);
    let origin = if checkout_ok { git_origin(&path) } else { None };
    let plan = plan_worktree(exists, checkout_ok, origin.as_deref(), owner, repo);
    let slug = format!("{owner}/{repo}");
    let mut outcome = match plan {
        WorktreePlan::Reuse => {
            let fetch = git_step(
                &path,
                &["fetch".into(), "origin".into(), "--prune".into()],
                PromptMode::Tty,
                SetupStep {
                    intent: format!("{slug} の既存クローンを origin の最新に揃えるため"),
                    command: String::new(),
                    desired_state: format!(
                        "{slug} のクローン {} が origin を fetch 済みである状態にしてから作業して",
                        path.display()
                    ),
                },
            );
            match fetch {
                Ok(_) => PrepareOutcome {
                    path,
                    note: "reused clone".into(),
                    failures: Vec::new(),
                },
                Err(failure) if git_ok(&path) => PrepareOutcome {
                    path,
                    note: "reused clone (fetch failed)".into(),
                    failures: vec![failure],
                },
                Err(failure) => {
                    match reclone(&path, &slug, "fetch failed and the checkout is unusable") {
                        Ok(ok) => ok,
                        Err(again) => PrepareOutcome {
                            path: best_workdir(&path, &path, home),
                            note: "clone failed".into(),
                            failures: vec![failure, again],
                        },
                    }
                }
            }
        }
        WorktreePlan::Clone => match clone_into(&path, &slug, "cloned") {
            Ok(ok) => ok,
            Err(failure) => PrepareOutcome {
                path: best_workdir(&path, &path, home),
                note: "clone failed".into(),
                failures: vec![failure],
            },
        },
        WorktreePlan::Reclone { reason } => match reclone(&path, &slug, &reason) {
            Ok(ok) => ok,
            Err(failure) => PrepareOutcome {
                path: best_workdir(&path, &path, home),
                note: "reclone failed".into(),
                failures: vec![failure],
            },
        },
    };
    if git_ok(&outcome.path) {
        if let Err(failure) = park_main_clone(&outcome.path) {
            outcome.failures.push(failure);
        }
    }
    outcome
}

/// The main clone stays on the default branch, or detached HEAD. It does not
/// keep a pull-request branch checked out.
pub fn park_main_clone(path: &Path) -> Result<(), SetupFailure> {
    let Some(branch) = current_branch(path) else {
        return Ok(());
    };
    if default_branch_name(path).as_deref() == Some(branch.as_str()) {
        return Ok(());
    }
    release_branch(
        path,
        &branch,
        "メインのクローン",
        &format!(
            "メインのクローン {} をデフォルトブランチまたは detached HEAD にしてから作業して",
            path.display()
        ),
    )
}

fn reclone(path: &Path, slug: &str, reason: &str) -> Result<PrepareOutcome, SetupFailure> {
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
        if let Err(err) = fs::rename(path, &aside) {
            return Err(step_failure(
                format!("{slug} の壊れたチェックアウトを脇へ退けるため"),
                format!("mv {} {}", path.display(), aside.display()),
                format!(
                    "{slug} を {} にクローンし直してから作業して",
                    path.display()
                ),
                1,
                format!("{err}"),
            ));
        }
    }
    let mut outcome = clone_into(path, slug, "recloned")?;
    outcome.note = format!("recloned ({reason})");
    let retired = retire_broken_worktrees(path);
    if !retired.is_empty() {
        outcome.note.push_str("; ");
        outcome.note.push_str(&retired.join("; "));
    }
    Ok(outcome)
}

/// After a reclone, per-PR worktrees still point at the old git dir and
/// `git status` exits 128. Try `git worktree repair`, then move anything
/// still broken aside so the next checkout can create it again.
pub fn retire_broken_worktrees(main: &Path) -> Vec<String> {
    let Some(name) = main.file_name() else {
        return Vec::new();
    };
    let wt_root = main.with_file_name(format!("{}.wt", name.to_string_lossy()));
    if !wt_root.is_dir() {
        return Vec::new();
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut notes = Vec::new();
    let entries = match fs::read_dir(&wt_root) {
        Ok(entries) => entries,
        Err(_) => return notes,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.contains(".broken-") {
            continue;
        }
        // Another ghost holds this slot. Do not repair or rename it out from under them.
        let Ok(_held) = lock_slot(&path) else {
            continue;
        };
        let path_s = path.display().to_string();
        let _ = git(main, &["worktree".into(), "repair".into(), path_s.clone()]);
        let _ = git(&path, &["worktree".into(), "repair".into()]);
        if git_ok(&path) {
            continue;
        }
        let aside = wt_root.join(format!("{file_name}.broken-{stamp}"));
        if fs::rename(&path, &aside).is_ok() {
            notes.push(format!(
                "moved broken worktree {} aside so it can be recreated",
                path.display()
            ));
        }
    }
    let _ = git(main, &["worktree".into(), "prune".into()]);
    notes
}

fn clone_into(path: &Path, slug: &str, verb: &str) -> Result<PrepareOutcome, SetupFailure> {
    let desired = format!(
        "{slug} のクローンが {} にある状態にしてから作業して",
        path.display()
    );
    let intent = format!("{slug} をローカルの作業ディレクトリにクローンするため");
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return Err(step_failure(
                intent,
                format!("mkdir -p {}", parent.display()),
                desired,
                1,
                format!("{err}"),
            ));
        }
    }
    if path.exists() {
        if let Err(err) = fs::remove_dir_all(path) {
            return Err(step_failure(
                intent,
                format!("rm -rf {}", path.display()),
                desired,
                1,
                format!("{err}"),
            ));
        }
    }
    let args = [
        "repo".into(),
        "clone".into(),
        slug.to_string(),
        path.display().to_string(),
    ];
    let command = format!(
        "gh repo clone {slug} {}",
        quote(&path.display().to_string())
    );
    let out = match shell::gh_tty(&args, None, Some(slug)) {
        Ok(out) => out,
        Err(err) => {
            return Err(step_failure(
                intent,
                command,
                desired,
                1,
                format!("{err:#}"),
            ))
        }
    };
    if out.code != 0 {
        let detail = if out.stderr.trim().is_empty() {
            out.stdout
        } else {
            out.stderr
        };
        return Err(step_failure(
            intent,
            command,
            desired,
            out.code,
            shell::with_credential_hint(&detail),
        ));
    }
    Ok(PrepareOutcome {
        path: path.to_path_buf(),
        note: verb.into(),
        failures: Vec::new(),
    })
}

#[derive(Debug, Clone)]
pub enum Checkout {
    Pr { number: u64, branch: Option<String> },
    Issue { branch: String, base: String },
}

#[derive(Debug, Clone)]
pub struct CheckoutOutcome {
    pub workdir: PathBuf,
    pub note: String,
    pub failures: Vec<SetupFailure>,
}

/// Fetch the PR head and check that branch out in `dest`, never in `main`.
/// A failed command becomes a [`SetupFailure`] and this returns the best
/// directory that still exists.
pub fn sync_and_checkout(
    main: &Path,
    dest: &Path,
    _slug: &str,
    checkout: Checkout,
    mode: PromptMode,
    purpose: &str,
) -> CheckoutOutcome {
    match checkout {
        Checkout::Pr { number, branch } => {
            checkout_pr(main, dest, number, branch.as_deref(), mode, purpose)
        }
        Checkout::Issue { branch, base } => {
            checkout_issue(main, dest, &branch, &base, mode, purpose)
        }
    }
}

fn checkout_pr(
    main: &Path,
    dest: &Path,
    number: u64,
    branch: Option<&str>,
    mode: PromptMode,
    purpose: &str,
) -> CheckoutOutcome {
    let desired = end_state(purpose, branch, dest);
    let _ = git_step(
        main,
        &["worktree".into(), "prune".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!(
                "{purpose} の worktree を追加する前に、壊れた worktree 登録を捨てるため"
            ),
            command: String::new(),
            desired_state: desired.clone(),
        },
    );
    let fetch_args = [
        "fetch".into(),
        "origin".into(),
        format!("pull/{number}/head"),
    ];
    if let Err(failure) = git_step(
        main,
        &fetch_args,
        mode,
        SetupStep {
            intent: format!("{purpose} の head を取得するため"),
            command: String::new(),
            desired_state: desired.clone(),
        },
    ) {
        return finish(main, dest, Vec::new(), vec![failure]);
    }
    let pr_ref = format!("refs/remotes/origin/pr/{number}");
    if let Err(failure) = git_step(
        main,
        &["update-ref".into(), pr_ref.clone(), "FETCH_HEAD".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!("{purpose} の head を共有 ref に残すため"),
            command: String::new(),
            desired_state: desired.clone(),
        },
    ) {
        return finish(main, dest, Vec::new(), vec![failure]);
    }
    // Do not fetch `refs/heads/{headRef}` into `refs/remotes/origin/{headRef}`.
    // A fork whose head is also named `main` would move origin/main.
    let _ = branch;
    match place_pr(main, dest, number, &pr_ref, purpose, &desired) {
        Ok(note) => finish(main, dest, note, Vec::new()),
        Err(failure) => finish(main, dest, Vec::new(), vec![failure]),
    }
}

fn checkout_issue(
    main: &Path,
    dest: &Path,
    branch: &str,
    base: &str,
    mode: PromptMode,
    purpose: &str,
) -> CheckoutOutcome {
    let desired = end_state(purpose, Some(branch), dest);
    let _ = git_step(
        main,
        &["worktree".into(), "prune".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!(
                "{purpose} の worktree を追加する前に、壊れた worktree 登録を捨てるため"
            ),
            command: String::new(),
            desired_state: desired.clone(),
        },
    );
    if let Err(failure) = git_step(
        main,
        &["fetch".into(), "origin".into(), base.to_string()],
        mode,
        SetupStep {
            intent: format!("{purpose} の base {base} を取得するため"),
            command: String::new(),
            desired_state: desired.clone(),
        },
    ) {
        if !ref_exists(main, &format!("refs/remotes/origin/{base}")) {
            return finish(main, dest, Vec::new(), vec![failure]);
        }
    }
    let _ = git_step(
        main,
        &[
            "fetch".into(),
            "origin".into(),
            format!("refs/heads/{branch}:refs/remotes/origin/{branch}"),
        ],
        mode,
        SetupStep {
            intent: format!("{purpose} のブランチ {branch} を追跡できるようにするため"),
            command: String::new(),
            desired_state: desired.clone(),
        },
    );
    if let Err(failure) = release_if_held(main, branch, purpose, &desired) {
        return finish(main, dest, Vec::new(), vec![failure]);
    }
    let start = if ref_exists(main, &format!("refs/remotes/origin/{branch}")) {
        format!("origin/{branch}")
    } else {
        format!("origin/{base}")
    };
    match place_issue(main, dest, branch, &start, purpose, &desired) {
        Ok(note) => finish(main, dest, note, Vec::new()),
        Err(failure) => finish(main, dest, Vec::new(), vec![failure]),
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

fn git_step(
    dir: &Path,
    args: &[String],
    mode: PromptMode,
    mut step: SetupStep,
) -> Result<CmdOut, SetupFailure> {
    if step.command.is_empty() {
        step.command = git_command(dir, args);
    }
    let result = match mode {
        PromptMode::Tty => shell::git_tty(dir, args),
        PromptMode::Batch => shell::git(dir, args),
    };
    match result {
        Err(err) => Err(SetupFailure::from_step(step, 1, format!("{err:#}"))),
        Ok(out) if out.code != 0 => {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.clone()
            } else {
                out.stderr.clone()
            };
            Err(SetupFailure::from_step(
                step,
                out.code,
                shell::with_credential_hint(&detail),
            ))
        }
        Ok(out) => Ok(out),
    }
}

pub fn best_workdir(dest: &Path, main: &Path, home: &Path) -> PathBuf {
    if git_ok(dest) {
        return dest.to_path_buf();
    }
    if git_ok(main) {
        return main.to_path_buf();
    }
    let work = home.join("work");
    if work.is_dir() {
        work
    } else if home.is_dir() {
        home.to_path_buf()
    } else {
        main.to_path_buf()
    }
}

fn finish(
    main: &Path,
    dest: &Path,
    notes: Vec<String>,
    failures: Vec<SetupFailure>,
) -> CheckoutOutcome {
    let home = main
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| main.to_path_buf());
    let workdir = if failures.is_empty() && git_ok(dest) {
        dest.to_path_buf()
    } else {
        best_workdir(dest, main, &home)
    };
    CheckoutOutcome {
        workdir,
        note: notes
            .into_iter()
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        failures,
    }
}

fn end_state(purpose: &str, branch: Option<&str>, dest: &Path) -> String {
    match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(branch) => format!(
            "{purpose} のブランチ {branch} を {} で作業できる状態にしてから作業して",
            dest.display()
        ),
        None => format!(
            "{purpose} を {} で作業できる状態にしてから作業して",
            dest.display()
        ),
    }
}

/// Local branch for a PR worktree. The directory name (`pr-N`, `pr-N-review`,
/// `pr-N-code`) is not the GitHub head ref, so it cannot be `origin/main`.
pub fn local_pr_branch(dest: &Path, number: u64) -> String {
    dest.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .unwrap_or_else(|| format!("pr-{number}"))
}

fn step_failure(
    intent: impl Into<String>,
    command: impl Into<String>,
    desired_state: impl Into<String>,
    exit_code: i32,
    stderr: impl Into<String>,
) -> SetupFailure {
    SetupFailure::from_step(
        SetupStep {
            intent: intent.into(),
            command: command.into(),
            desired_state: desired_state.into(),
        },
        exit_code,
        stderr,
    )
}

fn release_if_held(
    main: &Path,
    branch: &str,
    purpose: &str,
    desired: &str,
) -> Result<(), SetupFailure> {
    let Some(holder) = holder_of(main, branch, purpose, desired)? else {
        return Ok(());
    };
    if same_path(&holder, main) {
        release_branch(main, branch, purpose, desired)?;
    }
    Ok(())
}

fn release_branch(
    path: &Path,
    branch: &str,
    purpose: &str,
    desired: &str,
) -> Result<(), SetupFailure> {
    if current_branch(path).as_deref() != Some(branch) {
        return Ok(());
    }
    if dirty(path, purpose)? {
        let msg = format!("ghost: detach main clone off {branch}");
        git_step(
            path,
            &[
                "stash".into(),
                "push".into(),
                "--include-untracked".into(),
                "-m".into(),
                msg,
            ],
            PromptMode::Batch,
            SetupStep {
                intent: format!(
                    "{purpose} のブランチ {branch} を外す前に、未コミットの変更を退避するため"
                ),
                command: String::new(),
                desired_state: desired.to_string(),
            },
        )?;
    }
    if let Some(def) = default_branch_name(path) {
        if def != branch {
            let switched = git_step(
                path,
                &["checkout".into(), def],
                PromptMode::Batch,
                SetupStep {
                    intent: format!("{purpose} のブランチ {branch} をメインクローンから外すため"),
                    command: String::new(),
                    desired_state: desired.to_string(),
                },
            );
            if switched.is_ok() {
                return Ok(());
            }
        }
    }
    git_step(
        path,
        &["checkout".into(), "--detach".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!("{purpose} のブランチ {branch} をメインクローンから外すため"),
            command: String::new(),
            desired_state: desired.to_string(),
        },
    )?;
    Ok(())
}

fn place_pr(
    main: &Path,
    dest: &Path,
    number: u64,
    pr_ref: &str,
    purpose: &str,
    desired: &str,
) -> Result<Vec<String>, SetupFailure> {
    let branch = local_pr_branch(dest, number);
    if let Some(holder) = holder_of(main, &branch, purpose, desired)? {
        if same_path(&holder, dest) || same_slot(&holder, dest) {
            let note = update_existing(&holder, &branch, pr_ref, purpose, desired)?;
            clear_upstream(dest);
            return Ok(note);
        }
        if same_path(&holder, main) {
            release_branch(main, &branch, purpose, desired)?;
        } else {
            return Err(step_failure(
                format!("{purpose} のブランチを作業用 worktree に用意するため"),
                git_command(
                    main,
                    &["worktree".into(), "list".into(), "--porcelain".into()],
                ),
                desired,
                128,
                format!(
                    "fatal: '{branch}' is already used by worktree at '{}'",
                    holder.display()
                ),
            ));
        }
    }
    if dest.join(".git").exists() {
        let note = update_existing(dest, &branch, pr_ref, purpose, desired)?;
        clear_upstream(dest);
        return Ok(note);
    }
    // Start at the PR head. Never at origin/{headRef}, and never --track.
    add_or_checkout(main, dest, Some(&branch), pr_ref, false, purpose, desired)?;
    if ref_exists(main, &format!("refs/heads/{branch}")) {
        git_step(
            dest,
            &["merge".into(), "--ff-only".into(), pr_ref.to_string()],
            PromptMode::Batch,
            SetupStep {
                intent: format!("{purpose} のローカルブランチ {branch} を PR head まで進めるため"),
                command: String::new(),
                desired_state: desired.to_string(),
            },
        )?;
    }
    clear_upstream(dest);
    Ok(Vec::new())
}

fn clear_upstream(dest: &Path) {
    let _ = git(dest, &["branch".into(), "--unset-upstream".into()]);
}

/// Where a code-mode commit may be pushed. The local branch is never the ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodePush {
    Push { remote: String, refspec: String },
    ReadOnly { head_ref: String },
}

pub struct PushQuery<'a> {
    pub base_owner: &'a str,
    pub base_repo: &'a str,
    pub head_owner: &'a str,
    pub head_name: &'a str,
    pub head_ref: &'a str,
    pub head_url: &'a str,
    pub is_cross: bool,
    pub maintainer_can_modify: bool,
}

/// Same-repo pushes `origin HEAD:{headRef}`. A fork pushes the head repo URL
/// only when `maintainerCanModify`. Otherwise the plan is read-only.
pub fn plan_code_push(q: &PushQuery<'_>) -> CodePush {
    let head_ref = q.head_ref.trim();
    if head_ref.is_empty() {
        return CodePush::ReadOnly {
            head_ref: String::new(),
        };
    }
    if !q.is_cross {
        let _ = (q.base_owner, q.base_repo, q.head_owner, q.head_name);
        return CodePush::Push {
            remote: "origin".into(),
            refspec: format!("HEAD:{head_ref}"),
        };
    }
    if !q.maintainer_can_modify {
        return CodePush::ReadOnly {
            head_ref: head_ref.to_string(),
        };
    }
    CodePush::Push {
        remote: push_remote(q.head_owner, q.head_name, q.head_url),
        refspec: format!("HEAD:{head_ref}"),
    }
}

fn push_remote(owner: &str, name: &str, url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        // ssh, not https, so `--ssh-key` / GIT_SSH_COMMAND applies.
        return format!("git@github.com:{owner}/{name}.git");
    }
    if (url.starts_with("https://github.com/") || url.starts_with("http://github.com/"))
        && !url.ends_with(".git")
    {
        return format!("{url}.git");
    }
    url.to_string()
}

/// Run the planned push. Read-only does not invoke git.
pub fn apply_push(repo: &Path, plan: &CodePush) -> Result<(), String> {
    match plan {
        CodePush::ReadOnly { head_ref } => Err(format!("read-only: not pushing to {head_ref}")),
        CodePush::Push { remote, refspec } => {
            let out = git(repo, &["push".into(), remote.clone(), refspec.clone()])
                .map_err(|err| err.to_string())?;
            if out.code != 0 {
                Err(format!("{}{}", out.stdout, out.stderr))
            } else {
                Ok(())
            }
        }
    }
}

fn place_issue(
    main: &Path,
    dest: &Path,
    branch: &str,
    start: &str,
    purpose: &str,
    desired: &str,
) -> Result<Vec<String>, SetupFailure> {
    if let Some(holder) = holder_of(main, branch, purpose, desired)? {
        if !same_path(&holder, dest) && !same_slot(&holder, dest) {
            if same_path(&holder, main) {
                release_branch(main, branch, purpose, desired)?;
            } else {
                return Err(step_failure(
                    format!("{purpose} の作業ブランチを worktree に用意するため"),
                    git_command(
                        main,
                        &["worktree".into(), "list".into(), "--porcelain".into()],
                    ),
                    desired,
                    128,
                    format!(
                        "fatal: '{branch}' is already used by worktree at '{}'",
                        holder.display()
                    ),
                ));
            }
        }
    }
    let track = start.starts_with("origin/");
    if dest.join(".git").exists() {
        let mut notes = stash_dirty(dest, "autostash", purpose, desired)?;
        git_step(
            dest,
            &[
                "checkout".into(),
                "-B".into(),
                branch.to_string(),
                start.to_string(),
            ],
            PromptMode::Batch,
            SetupStep {
                intent: format!("{purpose} の作業ブランチを worktree に用意するため"),
                command: String::new(),
                desired_state: desired.to_string(),
            },
        )?;
        if !notes.is_empty() {
            notes = vec!["stashed local changes".into()];
        }
        return Ok(notes);
    }
    add_or_checkout(main, dest, Some(branch), start, track, purpose, desired)?;
    Ok(Vec::new())
}

fn add_or_checkout(
    main: &Path,
    dest: &Path,
    branch: Option<&str>,
    start: &str,
    track: bool,
    purpose: &str,
    desired: &str,
) -> Result<(), SetupFailure> {
    let intent = match branch {
        Some(_) => format!("{purpose} のブランチを作業用 worktree に用意するため"),
        None => format!("{purpose} の head を作業用 worktree に用意するため"),
    };
    if dest.join(".git").exists() {
        let args = match branch {
            Some(branch) if ref_exists(dest, &format!("refs/heads/{branch}")) => {
                vec!["checkout".into(), branch.to_string()]
            }
            Some(branch) if track => vec![
                "checkout".into(),
                "-b".into(),
                branch.to_string(),
                "--track".into(),
                start.to_string(),
            ],
            Some(branch) => vec![
                "checkout".into(),
                "-b".into(),
                branch.to_string(),
                start.to_string(),
            ],
            None => vec!["checkout".into(), "--detach".into(), start.to_string()],
        };
        git_step(
            dest,
            &args,
            PromptMode::Batch,
            SetupStep {
                intent,
                command: String::new(),
                desired_state: desired.to_string(),
            },
        )?;
        return Ok(());
    }
    move_aside(dest, &intent, desired)?;
    if let Some(parent) = dest.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            return Err(step_failure(
                intent,
                format!("mkdir -p {}", parent.display()),
                desired,
                1,
                format!("{err}"),
            ));
        }
    }
    let dest_s = dest.display().to_string();
    let args = match branch {
        Some(branch) if ref_exists(main, &format!("refs/heads/{branch}")) => {
            vec!["worktree".into(), "add".into(), dest_s, branch.to_string()]
        }
        Some(branch) if track => vec![
            "worktree".into(),
            "add".into(),
            "--track".into(),
            "-b".into(),
            branch.to_string(),
            dest_s,
            start.to_string(),
        ],
        Some(branch) => vec![
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.to_string(),
            dest_s,
            start.to_string(),
        ],
        None => vec![
            "worktree".into(),
            "add".into(),
            "--detach".into(),
            dest_s,
            start.to_string(),
        ],
    };
    git_step(
        main,
        &args,
        PromptMode::Batch,
        SetupStep {
            intent,
            command: String::new(),
            desired_state: desired.to_string(),
        },
    )?;
    Ok(())
}

fn update_existing(
    dest: &Path,
    branch: &str,
    pr_ref: &str,
    purpose: &str,
    desired: &str,
) -> Result<Vec<String>, SetupFailure> {
    let stashed = stash_dirty(dest, "autostash", purpose, desired)?;
    if current_branch(dest).as_deref() != Some(branch) {
        let args = if ref_exists(dest, &format!("refs/heads/{branch}")) {
            vec!["checkout".into(), branch.to_string()]
        } else {
            vec![
                "checkout".into(),
                "-b".into(),
                branch.to_string(),
                pr_ref.to_string(),
            ]
        };
        git_step(
            dest,
            &args,
            PromptMode::Batch,
            SetupStep {
                intent: format!("{purpose} のブランチを作業用 worktree に用意するため"),
                command: String::new(),
                desired_state: desired.to_string(),
            },
        )?;
    }
    git_step(
        dest,
        &["merge".into(), "--ff-only".into(), pr_ref.to_string()],
        PromptMode::Batch,
        SetupStep {
            intent: format!("{purpose} のブランチ {branch} を PR head まで進めるため"),
            command: String::new(),
            desired_state: desired.to_string(),
        },
    )?;
    Ok(stashed)
}

fn stash_dirty(
    path: &Path,
    message: &str,
    purpose: &str,
    desired: &str,
) -> Result<Vec<String>, SetupFailure> {
    if !dirty(path, purpose)? {
        return Ok(Vec::new());
    }
    git_step(
        path,
        &[
            "stash".into(),
            "push".into(),
            "--include-untracked".into(),
            "-m".into(),
            message.to_string(),
        ],
        PromptMode::Batch,
        SetupStep {
            intent: format!("{purpose} の作業ツリーに残った変更を退避するため"),
            command: String::new(),
            desired_state: desired.to_string(),
        },
    )?;
    Ok(vec!["stashed local changes".into()])
}

fn dirty(path: &Path, purpose: &str) -> Result<bool, SetupFailure> {
    let out = git_step(
        path,
        &["status".into(), "--porcelain".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!("{purpose} の作業ツリーがクリーンか確認するため"),
            command: String::new(),
            desired_state: format!(
                "{} の未コミット変更の有無が分かっている状態にする",
                path.display()
            ),
        },
    )?;
    Ok(!out.stdout.trim().is_empty())
}

fn move_aside(dest: &Path, intent: &str, desired: &str) -> Result<(), SetupFailure> {
    if !dest.exists() {
        return Ok(());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "wt".into());
    let aside = dest.with_file_name(format!("{name}.broken-{stamp}"));
    fs::rename(dest, &aside).map_err(|err| {
        step_failure(
            intent,
            format!("mv {} {}", dest.display(), aside.display()),
            desired,
            1,
            format!("{err}"),
        )
    })
}

fn holder_of(
    main: &Path,
    branch: &str,
    purpose: &str,
    desired: &str,
) -> Result<Option<PathBuf>, SetupFailure> {
    let out = git_step(
        main,
        &["worktree".into(), "list".into(), "--porcelain".into()],
        PromptMode::Batch,
        SetupStep {
            intent: format!(
                "{purpose} のブランチ {branch} がどこでチェックアウトされているか知るため"
            ),
            command: String::new(),
            desired_state: desired.to_string(),
        },
    )?;
    Ok(parse_worktree_porcelain(&out.stdout)
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(branch))
        .map(|entry| entry.path))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut path = None;
    let mut branch = None;
    let mut saw = false;
    let flush = |out: &mut Vec<WorktreeEntry>,
                 path: &mut Option<PathBuf>,
                 branch: &mut Option<String>,
                 saw: &mut bool| {
        if *saw {
            if let Some(path) = path.take() {
                out.push(WorktreeEntry {
                    path,
                    branch: branch.take(),
                });
            }
        }
        *path = None;
        *branch = None;
        *saw = false;
    };
    for line in text.lines() {
        if line.is_empty() {
            flush(&mut out, &mut path, &mut branch, &mut saw);
            continue;
        }
        saw = true;
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let rest = rest.trim();
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "detached" {
            branch = None;
        }
    }
    flush(&mut out, &mut path, &mut branch, &mut saw);
    out
}

fn current_branch(path: &Path) -> Option<String> {
    let out = git(
        path,
        &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
    )
    .ok()?;
    if out.code != 0 {
        return None;
    }
    let name = out.stdout.trim();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name.to_string())
    }
}

fn default_branch_name(path: &Path) -> Option<String> {
    let out = git(
        path,
        &[
            "symbolic-ref".into(),
            "--short".into(),
            "refs/remotes/origin/HEAD".into(),
        ],
    )
    .ok()?;
    if out.code != 0 {
        return None;
    }
    let name = out.stdout.trim();
    let short = name.strip_prefix("origin/").unwrap_or(name);
    if short.is_empty() {
        None
    } else {
        Some(short.to_string())
    }
}

fn ref_exists(path: &Path, refname: &str) -> bool {
    git(
        path,
        &[
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            refname.to_string(),
        ],
    )
    .map(|out| out.code == 0)
    .unwrap_or(false)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn same_slot(holder: &Path, dest: &Path) -> bool {
    holder.file_name() == dest.file_name()
        && holder.parent().and_then(|p| p.file_name()) == dest.parent().and_then(|p| p.file_name())
}

fn git_command(dir: &Path, args: &[String]) -> String {
    let mut parts = vec![
        "git".to_string(),
        "-C".into(),
        quote(&dir.display().to_string()),
    ];
    parts.extend(args.iter().map(|arg| quote(arg)));
    parts.join(" ")
}

fn quote(text: &str) -> String {
    if text.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | ':' | ',')
    }) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
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
        .truncate(false)
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
            issue_branch(12, "Crash on empty config", &[]),
            "fix/12-crash-on-empty-config"
        );
        assert_eq!(issue_branch(12, "クラッシュする", &[]), "fix/12");
        assert_eq!(issue_branch(3, "Add a picker", &[]), "feat/3-add-a-picker");
        assert_eq!(
            issue_branch(1, "Add fixture loader", &[]),
            "feat/1-add-fixture-loader"
        );
        assert!(issue_branch(9, &"word ".repeat(30), &[]).len() <= "feat/9-".len() + 40);
        // A docs mention in the body used to win. The title (and labels) decide.
        assert_eq!(
            issue_branch(2, "Fix crash on empty config", &[]),
            "fix/2-fix-crash-on-empty-config"
        );
        assert_eq!(
            issue_branch(5, "Update the overview", &["docs"]),
            "docs/5-update-the-overview"
        );
        assert_eq!(
            issue_branch(6, "Something odd", &["bug"]),
            "fix/6-something-odd"
        );
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
    fn broken_slot_worktree_is_moved_aside_so_it_can_be_recreated() {
        let root = std::env::temp_dir().join(format!(
            "ghost-repair-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = root.join("acme__app");
        let wt_root = root.join("acme__app.wt");
        let slot = wt_root.join("pr-2");
        fs::create_dir_all(&main).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "ghost")
                .env("GIT_AUTHOR_EMAIL", "ghost@example.com")
                .env("GIT_COMMITTER_NAME", "ghost")
                .env("GIT_COMMITTER_EMAIL", "ghost@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&root, &["init", "-b", "main", "acme__app"]);
        fs::write(main.join("f"), "a\n").unwrap();
        git(&main, &["add", "f"]);
        git(&main, &["commit", "-m", "init"]);
        fs::create_dir_all(&wt_root).unwrap();
        git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "fix/2-crash",
                slot.to_str().unwrap(),
            ],
        );
        assert!(git_ok(&slot));
        fs::write(slot.join(".git"), "gitdir: /no/such/gitdir\n").unwrap();
        assert!(!git_ok(&slot), "broken gitdir should fail rev-parse");
        let repaired = retire_broken_worktrees(&main);
        assert!(
            repaired.is_empty(),
            "a registered worktree is repaired in place: {repaired:?}"
        );
        assert!(git_ok(&slot), "repair rewrites the gitdir");

        let orphan = wt_root.join("pr-9");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join(".git"), "gitdir: /no/such/gitdir\n").unwrap();
        fs::write(orphan.join("leftover"), "keep\n").unwrap();
        assert!(!git_ok(&orphan));
        let notes = retire_broken_worktrees(&main);
        assert!(
            notes.iter().any(|n| n.contains("pr-9")),
            "unregistered orphan is moved aside: {notes:?}"
        );
        assert!(!orphan.exists(), "orphan path is free for a new worktree");
        assert!(
            wt_root.read_dir().unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pr-9.broken-")
            }),
            "the old checkout is kept aside"
        );
        assert!(git_ok(&slot), "repaired slot stays usable");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_locked_broken_slot_is_left_in_place() {
        let root = std::env::temp_dir().join(format!(
            "ghost-repair-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = root.join("acme__app");
        let wt_root = root.join("acme__app.wt");
        let orphan = wt_root.join("pr-9");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join(".git"), "gitdir: /no/such/gitdir\n").unwrap();
        let held = lock_slot(&orphan).unwrap();
        let notes = retire_broken_worktrees(&main);
        assert!(
            notes.iter().all(|note| !note.contains("pr-9")),
            "a held lock is not moved: {notes:?}"
        );
        assert!(orphan.exists(), "the locked slot stays where it is");
        assert!(orphan.join("leftover").exists() || orphan.join(".git").exists());
        drop(held);
        let notes = retire_broken_worktrees(&main);
        assert!(
            notes.iter().any(|note| note.contains("pr-9")),
            "once the lock is gone the broken slot can be moved: {notes:?}"
        );
        assert!(!orphan.exists());
        let _ = fs::remove_dir_all(&root);
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

    #[test]
    fn porcelain_keeps_slashed_branch_names() {
        let text = "\
worktree /tmp/main
HEAD abc
branch refs/heads/ghost/issue-1

worktree /tmp/wt/pr-2
HEAD def
detached
";
        let entries = parse_worktree_porcelain(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].branch.as_deref(), Some("ghost/issue-1"));
        assert_eq!(entries[0].path, PathBuf::from("/tmp/main"));
        assert!(entries[1].branch.is_none());
    }

    #[test]
    fn best_workdir_falls_back_to_ghost_work() {
        let root = std::env::temp_dir().join(format!(
            "ghost-best-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("ghost");
        let work = home.join("work");
        fs::create_dir_all(&work).unwrap();
        let dest = root.join("missing-slot");
        let main = root.join("missing-main");
        assert_eq!(best_workdir(&dest, &main, &home), work);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn setup_failure_is_structured_and_does_not_abort() {
        let root = std::env::temp_dir().join(format!(
            "ghost-setup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = root.join("nope");
        fs::create_dir_all(&main).unwrap();
        let dest = root.join("wt").join("pr-2");
        let outcome = sync_and_checkout(
            &main,
            &dest,
            "acme/app",
            Checkout::Pr {
                number: 2,
                branch: Some("ghost/issue-1".into()),
            },
            PromptMode::Batch,
            "レビュー対象の PR #2",
        );
        assert!(
            !outcome.failures.is_empty(),
            "a broken setup returns failures instead of stopping"
        );
        let failure = &outcome.failures[0];
        assert!(
            failure.intent.contains("レビュー対象の PR #2"),
            "{}",
            failure.intent
        );
        assert!(failure.intent.contains("ため"), "{}", failure.intent);
        assert!(failure.command.starts_with("git "), "{}", failure.command);
        assert_ne!(failure.exit_code, 0);
        assert!(!failure.stderr.trim().is_empty(), "{}", failure.stderr);
        assert!(
            failure.desired_state.contains("ghost/issue-1"),
            "{}",
            failure.desired_state
        );
        assert!(
            failure.desired_state.contains("作業できる状態"),
            "{}",
            failure.desired_state
        );
        assert!(outcome.workdir.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn old_main_checkout_of_ghost_issue_branch_moves_into_the_pr_worktree() {
        let root = std::env::temp_dir().join(format!(
            "ghost-oldbr-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let origin = root.join("origin.git");
        let main = root.join("main");
        let git_at = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "{args:?} in {}", dir.display());
        };
        fs::create_dir_all(&origin).unwrap();
        git_at(&origin, &["init", "--bare", "-b", "main"]);
        git_at(&root, &["clone", origin.to_str().unwrap(), "main"]);
        git_at(&main, &["config", "user.email", "t@example.com"]);
        git_at(&main, &["config", "user.name", "t"]);
        fs::write(main.join("f"), "base\n").unwrap();
        git_at(&main, &["add", "f"]);
        git_at(&main, &["commit", "-m", "init"]);
        git_at(&main, &["push", "-u", "origin", "main"]);
        git_at(&main, &["checkout", "-b", "ghost/issue-1"]);
        fs::write(main.join("f"), "pr\n").unwrap();
        git_at(&main, &["commit", "-am", "pr"]);
        git_at(&main, &["push", "-u", "origin", "ghost/issue-1"]);
        git_at(
            &origin,
            &["update-ref", "refs/pull/2/head", "refs/heads/ghost/issue-1"],
        );
        git_at(&main, &["remote", "set-head", "origin", "main"]);
        fs::write(main.join("dirty.txt"), "local\n").unwrap();

        let dest = root.join("wt").join("pr-2");
        let outcome = sync_and_checkout(
            &main,
            &dest,
            "acme/app",
            Checkout::Pr {
                number: 2,
                branch: Some("ghost/issue-1".into()),
            },
            PromptMode::Batch,
            "レビュー対象の PR #2",
        );
        assert!(
            outcome.failures.is_empty(),
            "{:?}",
            outcome
                .failures
                .iter()
                .map(|f| format!("{} :: {} :: {}", f.command, f.exit_code, f.stderr))
                .collect::<Vec<_>>()
        );
        assert_eq!(outcome.workdir, dest);
        assert_eq!(abbrev(&dest), "pr-2");
        assert_eq!(
            rev_parse(&dest, "HEAD"),
            rev_parse(&origin, "refs/pull/2/head")
        );
        assert_eq!(abbrev(&main), "ghost/issue-1");
        assert_eq!(
            rev_parse(&origin, "refs/heads/main"),
            rev_parse(&main, "main")
        );
        let upstream = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ])
            .current_dir(&dest)
            .output()
            .unwrap();
        assert!(
            !upstream.status.success(),
            "PR worktree must not track origin"
        );
        let stash = std::process::Command::new("git")
            .args(["stash", "list"])
            .current_dir(&main)
            .output()
            .unwrap();
        let stash = String::from_utf8_lossy(&stash.stdout);
        assert!(
            !stash.contains("ghost: detach"),
            "the main clone stays on the old branch: {stash}"
        );

        let again = sync_and_checkout(
            &main,
            &dest,
            "acme/app",
            Checkout::Pr {
                number: 2,
                branch: Some("ghost/issue-1".into()),
            },
            PromptMode::Batch,
            "レビュー対象の PR #2",
        );
        assert!(again.failures.is_empty(), "{:?}", again.failures);
        assert_eq!(again.workdir, dest);
        assert_eq!(abbrev(&dest), "pr-2");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fork_push_updates_the_fork_and_leaves_origin() {
        let root = std::env::temp_dir().join(format!(
            "ghost-fork-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let origin = root.join("origin.git");
        let fork = root.join("fork.git");
        let main = root.join("main");
        fs::create_dir_all(&origin).unwrap();
        let git_at = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "{args:?} in {}", dir.display());
        };
        git_at(&origin, &["init", "--bare", "-b", "main"]);
        git_at(&root, &["clone", origin.to_str().unwrap(), "main"]);
        git_at(&main, &["config", "user.email", "t@example.com"]);
        git_at(&main, &["config", "user.name", "t"]);
        fs::write(main.join("f"), "A\n").unwrap();
        git_at(&main, &["add", "f"]);
        git_at(&main, &["commit", "-m", "A"]);
        git_at(&main, &["push", "-u", "origin", "main"]);
        let sha_a = rev_parse(&origin, "refs/heads/main");
        git_at(
            &root,
            &["clone", "--bare", origin.to_str().unwrap(), "fork.git"],
        );
        fs::write(main.join("f"), "B\n").unwrap();
        git_at(&main, &["commit", "-am", "B"]);
        git_at(&main, &["push", "origin", "HEAD:refs/pull/4/head"]);
        let dest = root.join("wt").join("pr-4-code");
        let outcome = sync_and_checkout(
            &main,
            &dest,
            "acme/app",
            Checkout::Pr {
                number: 4,
                branch: Some("main".into()),
            },
            PromptMode::Batch,
            "コード対象の PR #4",
        );
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(abbrev(&dest), "pr-4-code");
        assert_ne!(rev_parse(&dest, "HEAD"), sha_a);
        assert_eq!(rev_parse(&origin, "refs/heads/main"), sha_a);
        fs::write(dest.join("f"), "C\n").unwrap();
        git_at(&dest, &["commit", "-am", "C"]);
        let plan = plan_code_push(&PushQuery {
            base_owner: "acme",
            base_repo: "app",
            head_owner: "forker",
            head_name: "app",
            head_ref: "main",
            head_url: fork.to_str().unwrap(),
            is_cross: true,
            maintainer_can_modify: true,
        });
        apply_push(&dest, &plan).unwrap();
        assert_eq!(
            rev_parse(&fork, "refs/heads/main"),
            rev_parse(&dest, "HEAD")
        );
        assert_eq!(rev_parse(&origin, "refs/heads/main"), sha_a);
        let fork_sha = rev_parse(&fork, "refs/heads/main");
        let blocked = plan_code_push(&PushQuery {
            base_owner: "acme",
            base_repo: "app",
            head_owner: "forker",
            head_name: "app",
            head_ref: "main",
            head_url: fork.to_str().unwrap(),
            is_cross: true,
            maintainer_can_modify: false,
        });
        assert!(
            matches!(blocked, CodePush::ReadOnly { .. }),
            "a fork without maintainerCanModify must not be a push, got {blocked:?}"
        );
        assert!(apply_push(&dest, &blocked).is_err());
        assert_eq!(
            rev_parse(&fork, "refs/heads/main"),
            fork_sha,
            "removing the maintainerCanModify check would push to the fork"
        );
        assert_eq!(rev_parse(&origin, "refs/heads/main"), sha_a);
        let ssh = plan_code_push(&PushQuery {
            base_owner: "acme",
            base_repo: "app",
            head_owner: "forker",
            head_name: "app",
            head_ref: "feature",
            head_url: "",
            is_cross: true,
            maintainer_can_modify: true,
        });
        match ssh {
            CodePush::Push { remote, refspec } => {
                assert_eq!(remote, "git@github.com:forker/app.git");
                assert_eq!(refspec, "HEAD:feature");
            }
            CodePush::ReadOnly { .. } => panic!("maintainer can modify; push the ssh url"),
        }
        let same = plan_code_push(&PushQuery {
            base_owner: "acme",
            base_repo: "app",
            head_owner: "acme",
            head_name: "app",
            head_ref: "main",
            head_url: "",
            is_cross: false,
            maintainer_can_modify: true,
        });
        match same {
            CodePush::Push { remote, refspec } => {
                assert_eq!(remote, "origin");
                assert_eq!(refspec, "HEAD:main");
            }
            CodePush::ReadOnly { .. } => panic!("same-repo push must use origin"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn abbrev(path: &Path) -> String {
        rev_parse(path, "--abbrev-ref HEAD")
    }

    fn rev_parse(path: &Path, spec: &str) -> String {
        let mut args = vec!["rev-parse".to_string()];
        args.extend(spec.split_whitespace().map(|s| s.to_string()));
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "rev-parse {spec} in {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}
