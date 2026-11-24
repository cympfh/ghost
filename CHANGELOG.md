# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] - 2026-11-24

### Added

- Add a clickable goto-bottom button with hover feedback when the log view is scrolled above the latest output.

### Fixed

- Use GitHub CLI credentials for HTTPS Git operations during setup and agent turns, and disable Git username/password prompts.
- Exit before opening the TUI or preparing a checkout when GitHub CLI is missing or not authenticated.
- Require successful GitHub approval reviews instead of approval-equivalent comments before the agent reports approval and review mode exits.

(git commit: e2f01d0)

## [0.4.0] - 2026-10-27

### Added

- Add `--skip-approval` to allow merge readiness without an approving review while still requiring passing CI.
- Wake the coding agent when the CI state changes, including transitions between pending, success, and failure.

### Changed

- Tell the coding agent to reply to actionable review comments after addressing them.

### Fixed

- Preserve Kimi sessions when handing off from issue mode to a code worktree.
- Build skip-approval prompts from the effective merge policy so agent status matches ghost's structural checks.

(git commit: 7c8b711)

## [0.3.0] - 2026-10-26

### Added

- Add `--merge-no` to exit successfully without merging or asking for confirmation.
- Run formatting, tests, and builds in GitHub Actions.

### Changed

- Continue automatically after reviews and issue-to-code handoffs without requiring `--yes`.
- Require passing CI and an approving review before a pull request can become merge-ready.
- Let the coding agent assess CI and review sentiment while ghost continues to enforce structural merge-readiness checks.

### Removed

- Remove `--yes`; continuation is now automatic, and merge behavior is controlled by `--merge-yes` or `--merge-no`.

### Fixed

- Show user input as sent only after the agent turn succeeds, and keep it queued after a failure.

(git commit: 6cad696)

## [0.2.0] - 2026-10-26

### Added

- Kimi Code (`kimi`) as a selectable coding agent, including automatic detection, streaming JSON output, and session continuation.

### Fixed

- Keep large Kimi prompts out of process arguments by reading the complete task from a prompt file.
- Recover Kimi sessions after failed or interrupted turns that do not emit a resume hint.
