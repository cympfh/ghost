use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::shell::{self, CmdOut};

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

pub fn issue_branch(number: u64) -> String {
    format!("ghost/issue-{number}")
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
            match git(&path, &["fetch".into(), "origin".into(), "--prune".into()]) {
                Ok(out) if out.code == 0 => Ok((path, "reused clone".into())),
                Ok(out) if git_ok(&path) => Ok((
                    path,
                    format!(
                        "reused clone (fetch failed: {})",
                        one_line(&out.stderr, &out.stdout)
                    ),
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
    let out = shell::gh(
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
        anyhow::bail!(
            "gh repo clone {slug} failed: {}",
            one_line(&out.stderr, &out.stdout)
        );
    }
    Ok((path.to_path_buf(), verb.to_string()))
}

#[derive(Debug, Clone)]
pub enum Checkout {
    Pr(u64),
    Issue { number: u64, base: String },
}

pub fn sync_and_checkout(path: &Path, slug: &str, checkout: Checkout) -> Result<String> {
    let fetch = git(path, &["fetch".into(), "origin".into(), "--prune".into()])?;
    if fetch.code != 0 && !git_ok(path) {
        anyhow::bail!(
            "git fetch failed: {}",
            one_line(&fetch.stderr, &fetch.stdout)
        );
    }
    let mut note = String::new();
    let status = git(path, &["status".into(), "--porcelain".into()])?;
    if status.code == 0 && !status.stdout.trim().is_empty() {
        let stash = git(
            path,
            &[
                "stash".into(),
                "push".into(),
                "--include-untracked".into(),
                "-m".into(),
                "ghost autostash".into(),
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
            let out = shell::gh(
                &[
                    "pr".into(),
                    "checkout".into(),
                    number.to_string(),
                    "--repo".into(),
                    slug.to_string(),
                ],
                Some(path),
                Some(slug),
            )?;
            if out.code != 0 {
                anyhow::bail!(
                    "gh pr checkout {number} failed: {}",
                    one_line(&out.stderr, &out.stdout)
                );
            }
        }
        Checkout::Issue { number, base } => {
            let branch = issue_branch(number);
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
                anyhow::bail!(
                    "git checkout failed: {}",
                    one_line(&out.stderr, &out.stdout)
                );
            }
        }
    }
    Ok(note)
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
        anyhow::bail!("git diff failed: {}", one_line(&out.stderr, &out.stdout));
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
        assert_eq!(issue_branch(7), "ghost/issue-7");
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
}
