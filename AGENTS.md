# ghost contributor guide

## Project overview

`ghost` is a terminal application that supervises GitHub pull requests and issues with a local AI coding agent. It can review a pull request, fix CI failures and review comments, implement an issue and open a pull request, process issues automatically, and merge a pull request after confirmation or an explicit auto-merge option.

The executable is `ghost`. The package also exposes its implementation as a Rust library so the binary and unit tests share the same modules.

Key behavior:

- All GitHub operations go through the GitHub CLI (`gh`); there is no GitHub HTTP client or GitHub SDK dependency.
- One of `codex`, `claude`, `grok`, or `kimi` must be available at runtime. Automatic selection tries them in that order.
- A launch starts a fresh agent session. Later updates resume the session held by the current process rather than restoring a saved session from disk.
- Review, code, and issue flows use separate locked Git worktrees and persistent state.
- The TUI polls GitHub, streams agent output, accepts user input, and presents mode and merge confirmations.
- The default interface and prompt language is Japanese (`ja`); English (`en`) is also supported.

Read `README.md` for installation and common commands. `README.ja.md` is its Japanese counterpart. `docs/internals.md` is the detailed behavioral reference for flags, keys, polling, merging, state, worktrees, agent commands, and security limitations. Keep these documents synchronized with behavior changes.

## Technology stack and configuration

- Rust, edition 2021, using Cargo.
- `clap` with derive support for CLI parsing.
- `ratatui` and `crossterm` for the terminal UI.
- `anyhow` for error propagation and context.
- `serde` and `serde_json` for GitHub data, agent events, and persistent JSON state.
- `unicode-width` for terminal text layout.
- `libc` and Unix process APIs for process groups, signals, terminal restoration, and file locks.
- No async runtime: concurrency uses threads and `std::sync::mpsc` channels.

`Cargo.toml` and the committed `Cargo.lock` are the only package/build manifests. Dependencies are pinned to exact versions. The direct `instability`, `unicode-segmentation`, and `unicode-truncate` pins intentionally keep transitive dependencies compatible with Rust 1.83; preserve that compatibility when changing dependencies. There is no `rust-toolchain` file and no separate formatter or Clippy configuration.

The source uses Unix-specific imports and process behavior. Development and runtime should be treated as Unix-oriented, not portable Windows Rust.

Runtime requirements for a real run are `gh`, `git`, and at least one supported coding agent on `PATH`. Authenticate GitHub CLI with `gh auth login`. Building and running the unit tests does not require GitHub credentials or a real coding agent.

## Build and run commands

Run these from the repository root:

```sh
cargo build
cargo build --release
cargo fmt --all -- --check
cargo test
cargo run -- --help
```

Useful local TUI previews that do not need GitHub or an agent:

```sh
cargo run -- --demo picker
cargo run -- --demo review
cargo run -- --demo code
cargo run -- --demo confirm
cargo run -- --demo preflight
```

Install the binary directly from the repository as documented:

```sh
cargo install --git https://github.com/cympfh/ghost
```

There is no repository CI workflow, container definition, release script, or other deployment automation. Do not assume that a separate publishing or deployment step exists.

## Runtime architecture

The main execution path is:

1. `src/main.rs` calls `ghost::cli::run()` and converts its result into a process exit code.
2. `src/cli.rs` parses flags and the target URL, validates mode combinations, and constructs an `engine::RunSpec`.
3. `src/engine.rs` performs preflight checks and prepares the clone/worktree before entering terminal raw mode, so SSH can still ask for a passphrase.
4. `src/tui.rs` owns terminal input/output. A worker communicates with it through `UiEvent` and `UiCmd` channels.
5. The engine polls through `github::Gh`, uses decision logic in `watch`, `merge`, and `auto`, builds prompts with `prompt`, and dispatches turns through `agent::AgentSession`.
6. Successful turns update persistent state. Later polls call the agent only for relevant changes, except for the mandatory opening situation check and explicit user input.

Setup failures are structured records rather than immediate panics. Where possible, ghost keeps the UI running and hands the failed command, intent, output, and desired state to the agent. Missing `gh`, missing authentication, or no available agent are startup failures.

The process installs signal handling for terminal restoration and agent process-group cleanup. Preserve this cleanup when changing process or terminal lifecycle code.

## Source layout

All implementation files are directly under `src/`; there are no nested source modules or separate integration-test directories.

- `main.rs`: minimal binary entry point.
- `lib.rs`: exports the flat module tree.
- `cli.rs`: Clap definitions, flag/target validation, mode selection, environment overrides.
- `target.rs`: parsing and validation of repository, pull request, and issue URLs.
- `interval.rs`: polling-duration parser.
- `lang.rs`: Japanese/English selection and bilingual text helper.
- `engine.rs`: central orchestration, polling loops, session transitions, retries, user commands, and merge flow. This is the largest and most stateful module.
- `watch.rs`: pull-request snapshots and mostly pure decisions about when to review, fix, stop, trust comments, or suppress duplicate work.
- `merge.rs`: merge readiness, repeated-observation rules, merge answers, and stop reasons.
- `auto.rs`: oldest-first automatic issue selection, linked-PR handling, skip reasons, and transient retry policy.
- `github.rs`: `gh` command construction and JSON parsing for GitHub data and feedback.
- `shell.rs`: external command execution, timeouts, executable lookup, SSH environment, and prompt/batch modes.
- `preflight.rs`: classifies `gh` availability, authentication, and repository probes.
- `setup.rs`: structured setup-step failures and stderr trimming.
- `agent.rs`: coding-agent command lines, streaming JSON parsing, process/session lifecycle, and the `gh`/`git` wrapper scripts.
- `prompt.rs`: review, code, issue, and setup-handoff prompts, including untrusted-text wrapping.
- `workdir.rs`: clone reuse, worktree creation and locking, branch naming, checkouts, and push planning.
- `state.rs`: JSON state paths and persistence for reviewed SHAs, seen comments, sessions, CI signatures, and automatic-issue failures.
- `tui.rs`: Ratatui rendering, event loop, pickers, confirmations, and keyboard handling.
- `theme.rs`: centralized colors and `NO_COLOR` behavior. Use these helpers rather than introducing independent styling.
- `text.rs`: ANSI stripping and Unicode-aware wrapping, truncation, padding, and formatting.
- `demo.rs`: scripted TUI scenes used for previews and screenshots.
- `docs/internals.md`: detailed user-facing operational semantics.
- `docs/screenshots/`: TUI screenshots; `docs/logo.svg`: project logo.

## Development conventions

- Match the existing flat-module structure and public types. Keep orchestration in `engine.rs` and reusable parsing or decision logic in the appropriate focused module.
- Use `anyhow::Result` and `.context(...)` at I/O and process boundaries. Pure parsers and decision helpers commonly return concrete error types or `String` errors, following their surrounding code.
- Prefer typed enums and explicit state transitions for modes, actions, launch state, merge state, and setup failures.
- Keep GitHub access in the `gh` abstraction rather than adding direct API calls.
- Run `rustfmt` and keep the existing standard Rust style. There are no custom formatting rules.
- Comments and technical documentation are primarily English. User-facing strings and prompts are often bilingual; update both Japanese and English variants when changing behavior. Some setup intents and desired-state strings are intentionally Japanese even in shared code.
- Preserve exact machine-readable prompt markers such as `GHOST_VERDICT`, CLI error text covered by tests, and agent command argument ordering.
- Use `theme.rs` for TUI colors and `text.rs` for display-width operations. Secondary text intentionally uses ANSI gray, and a non-empty `NO_COLOR` disables color.
- Persistent state uses atomic temporary-file-plus-rename writes. State belongs outside the source checkout; do not write runtime state into this repository.
- Update `README.md`, `README.ja.md`, and `docs/internals.md` as applicable when changing user-visible behavior.

## Testing strategy

Tests are colocated in the same `src/*.rs` files under `#[cfg(test)] mod tests`. There is no top-level `tests/` directory and no dev-dependency test framework.

The suite emphasizes:

- URL, CLI, interval, JSON, and agent-event parsing.
- Pure review/code/merge/auto decision tables.
- Exact external command arguments and safety guards.
- State persistence and duplicate-work suppression.
- TUI rendering and non-interactive behavior.
- Temporary directories, shell-script agent/GitHub stubs, and local temporary Git repositories for process and worktree behavior.

Run the complete suite after changes:

```sh
cargo test
```

Run a focused test by name while iterating, for example:

```sh
cargo test parse_target
cargo test agent::tests::command_lines_continue_one_session
```

Add or adjust colocated tests alongside behavior changes, especially for pure decisions and exact command construction. Tests may invoke `git` and `/bin/sh` and may print local Git setup output; they do not need live GitHub access. Use temporary directories and stubs for new subprocess tests rather than real repositories, credentials, or coding agents.

For manual UI checks, use `--demo` scenes. For real end-to-end runs, use a trusted test repository because the agent has elevated permissions and code mode can push and merge.

## Files, state, and environment

- `GHOST_HOME` overrides `~/.ghost`.
- Clones live at `$GHOST_HOME/work/OWNER__REPO`.
- Per-mode worktrees live under `$GHOST_HOME/work/OWNER__REPO.wt/` as `pr-N-review`, `pr-N-code`, or `issue-N`.
- Prompt files and temporary agent wrappers live under `$GHOST_HOME/prompts/pid<PID>-<SLOT>/`.
- `GHOST_STATE_DIR` overrides the state root. Otherwise ghost uses `$XDG_STATE_HOME/ghost`, then `~/.local/state/ghost`.
- Per-repository state is under `OWNER__REPO/`, including PR/issue JSON, `auto.json`, `created-issues`, and `created-prs`.
- `GHOST_SSH_KEY` supplies the SSH key when `--ssh-key` is absent.
- `GHOST_LANG` selects `ja` or `en` when `--lang` is absent.
- A non-empty `NO_COLOR` disables TUI color.

Initial clone/fetch/checkout uses the normal terminal. Later git and agent processes disable interactive credential prompts. Worktrees and clones intentionally remain after exit.

## Security considerations

This program deliberately runs coding agents with their permission prompts disabled: Claude `--dangerously-skip-permissions`, Codex `--dangerously-bypass-approvals-and-sandbox`, Grok `--yolo`, or Kimi print mode's automatic permissions. `--auto` also acts on issue content without asking first. Use and test it only on repositories whose content and credentials you trust.

- Review titles, diffs, and comments are wrapped as untrusted data, but those markers are not a security boundary against prompt injection.
- `--trust-comments` controls which feedback wakes code mode and defaults to `all`. `--trust-issues` separately controls automatic issue selection and defaults to `collaborators`.
- The generated `gh` and `git` wrappers are accident guards, not sandboxing or authorization boundaries. Agents can bypass them with aliases, absolute executable paths, or other commands.
- Review mode refuses pushes through the wrapper. Code mode constrains pushes to the planned remote and refspec, including fork-specific handling. Do not weaken these checks without preserving their tested behavior.
- Agents are instructed never to merge. Ghost itself performs merges only through its merge flow, re-checking the head SHA and passing exactly one method plus `--match-head-commit`.
- Merge readiness still requires the same head to be observed ready on two checks at least one polling interval apart. `--merge-yes` does not bypass that delay.
- `-y`/`--yes` answers continuation and review-exit prompts; it does not authorize merging. `--merge-yes` is the separate auto-merge option.
- Do not print tokens or credential contents in logs or errors. Existing preflight error handling deliberately suppresses lines that appear to contain tokens.
- Avoid live GitHub operations in ordinary unit tests, especially pushes, merges, issue creation, or comments.
