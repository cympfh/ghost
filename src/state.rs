use std::fs;
use std::path::{Path, PathBuf};

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

/// Issues `--auto` already failed, so a later check does not take them again.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoState {
    #[serde(default)]
    pub failed: Vec<AutoFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoFailure {
    pub number: u64,
    pub reason: String,
}

impl AutoState {
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

    pub fn record(&mut self, number: u64, reason: &str) {
        let reason = reason.trim();
        let reason = if reason.chars().count() > 500 {
            let mut cut: String = reason.chars().take(500).collect();
            cut.push('…');
            cut
        } else {
            reason.to_string()
        };
        if let Some(row) = self.failed.iter_mut().find(|row| row.number == number) {
            row.reason = reason;
            return;
        }
        self.failed.push(AutoFailure { number, reason });
    }
}

pub fn auto_state_path(root: &Path, owner: &str, repo: &str) -> PathBuf {
    root.join(format!("{owner}__{repo}")).join("auto.json")
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
}
