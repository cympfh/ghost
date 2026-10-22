//! ghost babysits a GitHub pull request with one local coding-agent session.
//!
//! GitHub is polled with the `gh` CLI. The agent is started once and later
//! updates are follow-ups in that same session (`--resume` / `exec resume`).

pub mod agent;
pub mod cli;
pub mod demo;
pub mod engine;
pub mod github;
pub mod interval;
pub mod lang;
pub mod merge;
pub mod preflight;
pub mod prompt;
pub mod setup;
pub mod shell;
pub mod state;
pub mod target;
pub mod text;
pub mod theme;
pub mod tui;
pub mod watch;
pub mod workdir;
