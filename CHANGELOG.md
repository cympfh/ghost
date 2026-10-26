# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
