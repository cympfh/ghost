use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;

/// Persisted across restarts so ghost does not redo a review or re-feed comments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhostState {
    #[serde(default)]
    pub last_reviewed_sha: Option<String>,
    #[serde(default)]
    pub seen_comment_ids: Vec<String>,
    /// Session that posts reviews. Follow-ups resume this id.
    #[serde(default)]
    pub review_session_id: Option<String>,
    /// Session that writes code. Issue mode keeps this id when it becomes --code.
    #[serde(default)]
    pub code_session_id: Option<String>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    /// SHA whose failing-check set was already handed to the agent.
    #[serde(default)]
    pub handled_ci_sha: Option<String>,
    #[serde(default)]
    pub handled_failing_checks: Vec<String>,
    /// Comment ids written by the agent (same gh user) so they are not new work.
    #[serde(default)]
    pub agent_comment_ids: Vec<String>,
}

impl GhostState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            fs::read_to_string(path).with_context(|| format!("read state {}", path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(state) => Ok(state),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn session_for(&self, slot: SessionSlot, kind: AgentKind) -> Option<String> {
        if self.agent_kind.as_deref() != Some(kind.as_str()) {
            return None;
        }
        let id = match slot {
            SessionSlot::Review => self.review_session_id.as_deref(),
            SessionSlot::Code => self.code_session_id.as_deref(),
        };
        id.filter(|s| !s.is_empty()).map(|s| s.to_string())
    }

    pub fn remember_session(&mut self, slot: SessionSlot, kind: AgentKind, id: &str) {
        if id.is_empty() {
            return;
        }
        self.agent_kind = Some(kind.as_str().to_string());
        match slot {
            SessionSlot::Review => self.review_session_id = Some(id.to_string()),
            SessionSlot::Code => self.code_session_id = Some(id.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSlot {
    Review,
    Code,
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GHOST_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("ghost");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/ghost")
}

pub fn pr_slot_name(number: u64, slot: SessionSlot) -> String {
    match slot {
        SessionSlot::Review => format!("pr-{number}-review"),
        SessionSlot::Code => format!("pr-{number}-code"),
    }
}

pub fn pr_state_path(
    root: &Path,
    owner: &str,
    repo: &str,
    number: u64,
    slot: SessionSlot,
) -> PathBuf {
    root.join(format!("{owner}__{repo}"))
        .join(format!("{}.json", pr_slot_name(number, slot)))
}

pub fn issue_state_path(root: &Path, owner: &str, repo: &str, number: u64) -> PathBuf {
    root.join(format!("{owner}__{repo}"))
        .join(format!("issue-{number}.json"))
}

/// Issues `--auto` will not take again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoState {
    #[serde(default)]
    pub failed: Vec<AutoFailure>,
}

/// One load of `auto.json`. A corrupt file is renamed aside and `warning` explains where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoLoad {
    pub state: AutoState,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoFailure {
    pub number: u64,
    pub reason: String,
    /// A GitHub or network blip. Retried after `retry_at`, or sooner when `head` changes.
    #[serde(default)]
    pub transient: bool,
    #[serde(default)]
    pub retry_at: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub attempts: u32,
}

impl AutoState {
    pub fn load(path: &Path) -> Result<AutoLoad> {
        if !path.exists() {
            return Ok(AutoLoad {
                state: Self::default(),
                warning: None,
            });
        }
        let raw =
            fs::read_to_string(path).with_context(|| format!("read state {}", path.display()))?;
        match serde_json::from_str(&raw) {
            Ok(state) => Ok(AutoLoad {
                state,
                warning: None,
            }),
            Err(err) => {
                let backup = auto_backup_path(path);
                fs::rename(path, &backup).with_context(|| {
                    format!(
                        "auto.json at {} is corrupt ({err}) and could not be backed up to {}",
                        path.display(),
                        backup.display()
                    )
                })?;
                Ok(AutoLoad {
                    state: Self::default(),
                    warning: Some(format!(
                        "auto.json at {} is corrupt ({err}); backed it up to {}. A new file starts empty. Copy the backup back if you still need those skips.",
                        path.display(),
                        backup.display()
                    )),
                })
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn has_permanent(&self, number: u64) -> bool {
        self.failed
            .iter()
            .any(|row| row.number == number && !row.transient)
    }

    pub fn attempts(&self, number: u64) -> u32 {
        self.failed
            .iter()
            .find(|row| row.number == number)
            .map(|row| row.attempts)
            .unwrap_or(0)
    }

    pub fn record(
        &mut self,
        number: u64,
        reason: &str,
        transient: bool,
        retry_at: &str,
        head: &str,
    ) {
        let reason = clip_reason(reason);
        let attempts = self.attempts(number).saturating_add(1);
        let row = AutoFailure {
            number,
            reason,
            transient,
            retry_at: retry_at.to_string(),
            head: head.to_string(),
            attempts,
        };
        if let Some(existing) = self.failed.iter_mut().find(|row| row.number == number) {
            *existing = row;
            return;
        }
        self.failed.push(row);
    }
}

fn clip_reason(reason: &str) -> String {
    let reason = reason.trim();
    if reason.chars().count() > 500 {
        let mut cut: String = reason.chars().take(500).collect();
        cut.push('…');
        cut
    } else {
        reason.to_string()
    }
}

fn auto_backup_path(path: &Path) -> PathBuf {
    let bad = path.with_extension("json.bad");
    if !bad.exists() {
        return bad;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "auto.json".into());
    path.with_file_name(format!("{name}.bad-{secs}"))
}

pub fn auto_state_path(root: &Path, owner: &str, repo: &str) -> PathBuf {
    root.join(format!("{owner}__{repo}")).join("auto.json")
}

/// One issue number per line. The `gh` wrapper appends numbers from `gh issue create`.
pub fn created_issues_path(root: &Path, owner: &str, repo: &str) -> PathBuf {
    root.join(format!("{owner}__{repo}")).join("created-issues")
}

pub fn read_created_issues(path: &Path) -> Vec<u64> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut nums = Vec::new();
    for line in raw.lines() {
        let Ok(number) = line.trim().parse::<u64>() else {
            continue;
        };
        if number > 0 && !nums.contains(&number) {
            nums.push(number);
        }
    }
    nums
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_kind_guard() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = pr_state_path(&dir, "acme", "widgets", 3, SessionSlot::Code);
        assert!(path.ends_with("acme__widgets/pr-3-code.json"));
        let review = pr_state_path(&dir, "acme", "widgets", 3, SessionSlot::Review);
        assert!(review.ends_with("acme__widgets/pr-3-review.json"));
        assert_ne!(path, review);
        let mut state = GhostState {
            last_reviewed_sha: Some("abc123".into()),
            seen_comment_ids: vec!["c1".into()],
            ..GhostState::default()
        };
        state.remember_session(SessionSlot::Code, AgentKind::Claude, "sess-1");
        state.save(&path).unwrap();
        let loaded = GhostState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(
            loaded
                .session_for(SessionSlot::Code, AgentKind::Claude)
                .as_deref(),
            Some("sess-1")
        );
        assert!(loaded
            .session_for(SessionSlot::Code, AgentKind::Codex)
            .is_none());
        assert!(loaded
            .session_for(SessionSlot::Review, AgentKind::Claude)
            .is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_starts_fresh() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-state-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = issue_state_path(&dir, "a", "b", 1);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let loaded = GhostState::load(&path).unwrap();
        assert_eq!(loaded, GhostState::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_auto_json_is_backed_up_and_not_silently_reset() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-auto-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = auto_state_path(&dir, "acme", "app");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let loaded = AutoState::load(&path).unwrap();
        assert!(loaded.state.failed.is_empty());
        let warning = loaded.warning.expect("corrupt auto.json warns");
        assert!(warning.contains("corrupt"), "{warning}");
        assert!(warning.contains("acme__app"), "{warning}");
        let backup = path.with_extension("json.bad");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "{not json");
        assert!(!path.exists(), "the corrupt file is moved aside");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn created_issue_file_keeps_numbers_from_gh_issue_create() {
        let dir = std::env::temp_dir().join(format!(
            "ghost-created-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = created_issues_path(&dir, "acme", "app");
        assert!(read_created_issues(&path).is_empty());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "44\n\nbad\n81\n44\n").unwrap();
        assert_eq!(read_created_issues(&path), vec![44, 81]);
        let _ = fs::remove_dir_all(&dir);
    }
}
