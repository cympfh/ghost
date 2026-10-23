<p align="center"><img src="docs/logo.svg" width="120" alt="ghost"></p>

# ghost

[English](README.md) | [日本語](README.ja.md)

ghost has a local coding agent review a pull request, implement an issue, or fix CI failures and review comments. When the pull request is mergeable, it confirms with you and then merges itself.

## Requirements

You need the [GitHub CLI](https://cli.github.com/) and one of `codex`, `claude`, or `grok` on `PATH`. Log in with `gh auth login`.

```sh
cargo install --path .
```

## Usage

```sh
ghost github.com/OWNER/REPO                  # pick an open pull request or issue
ghost github.com/OWNER/REPO/pull/N --review  # review
ghost github.com/OWNER/REPO/pull/N --code    # fix CI failures and review comments
ghost github.com/OWNER/REPO/pulls --code     # pick an open pull request to fix
ghost github.com/OWNER/REPO/issues/N         # implement an issue, then keep fixing
ghost github.com/OWNER/REPO/issues --auto    # oldest open issue, then the next
```

The rest of the guide is in [docs/internals.md](docs/internals.md).
