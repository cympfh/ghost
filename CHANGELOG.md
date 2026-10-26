# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-26

### Added

- Kimi Code (`kimi`) as a selectable coding agent, including automatic detection, streaming JSON output, and session continuation.

### Fixed

- Keep large Kimi prompts out of process arguments by reading the complete task from a prompt file.
- Recover Kimi sessions after failed or interrupted turns that do not emit a resume hint.
