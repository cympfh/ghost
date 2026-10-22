//! One git/gh setup step, and the record we keep when it fails.
//!
//! A step is `{intent, command, desired_state}`. Intent and desired state are
//! Japanese: the handoff and the TUI warning quote them as written.

/// What ghost was trying to do before it ran `command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    /// Why this step exists. Japanese, typically ending in ため.
    pub intent: String,
    /// The exact command line ghost ran.
    pub command: String,
    /// The end state ghost wanted. Japanese.
    pub desired_state: String,
}

/// A setup step that did not reach `desired_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupFailure {
    pub intent: String,
    pub command: String,
    pub desired_state: String,
    pub exit_code: i32,
    pub stderr: String,
}

impl SetupFailure {
    pub fn from_step(step: SetupStep, exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            intent: step.intent,
            command: step.command,
            desired_state: step.desired_state,
            exit_code,
            stderr: trim_stderr(&stderr.into()),
        }
    }
}

/// Trim surrounding whitespace and keep the tail when stderr is huge.
pub fn trim_stderr(raw: &str) -> String {
    let text = raw.trim();
    const MAX: usize = 2_000;
    let count = text.chars().count();
    if count <= MAX {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - MAX).collect();
    format!("…[trimmed]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_keeps_the_step_and_trims_stderr() {
        let failure = SetupFailure::from_step(
            SetupStep {
                intent: "レビュー対象の PR #2 のブランチを作業用 worktree に用意するため".into(),
                command: "git -C /tmp/main fetch origin pull/2/head".into(),
                desired_state:
                    "レビュー対象の PR #2 のブランチ ghost/issue-1 を /tmp/wt で作業できる状態にしてから作業して"
                        .into(),
            },
            128,
            "\n fatal: 'ghost/issue-1' is already used by worktree at '/tmp/main' \n",
        );
        assert_eq!(
            failure.intent,
            "レビュー対象の PR #2 のブランチを作業用 worktree に用意するため"
        );
        assert_eq!(failure.command, "git -C /tmp/main fetch origin pull/2/head");
        assert!(failure.desired_state.contains("ghost/issue-1"));
        assert_eq!(failure.exit_code, 128);
        assert_eq!(
            failure.stderr,
            "fatal: 'ghost/issue-1' is already used by worktree at '/tmp/main'"
        );
        assert!(!failure.stderr.ends_with('\n'));
    }
}
