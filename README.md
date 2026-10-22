# ghost

[English](README.md) | [日本語](README.ja.md)

A TUI that babysits a GitHub pull request with one local coding agent (Codex, Claude, or Grok) until it is merge-ready.

![Reviewing a pull request](docs/screenshots/review.png)

## Install

You need the [GitHub CLI](https://cli.github.com/) (`gh auth login`) and one of `codex`, `claude`, or `grok` on `PATH`.

```sh
cargo install --path .
```

At startup ghost runs `gh auth status` and `gh repo view OWNER/REPO`. If either fails, the TUI explains the fix (`gh auth login`, install `gh`, or check the repo slug) and exits.

## Usage

```sh
ghost github.com/OWNER/REPO                  # picker of open PRs and issues
ghost github.com/OWNER/REPO/pr/N --review    # review when the head SHA changes
ghost github.com/OWNER/REPO/pr/N --code      # fix red CI and new review comments
ghost github.com/OWNER/REPO/issue/N          # implement, open a PR, then continue as --code
ghost github.com/OWNER/REPO/issue/N --yes    # same, accepting the continue prompt
```

`--interval` defaults to `20s` for review, code, and issue → code (`30s`, `2m`, `1h30m`, or a bare number of seconds). `--agent codex|claude|grok` overrides the default, which is the first of those on `PATH`. `--lang ja|en` defaults to `ja` (`GHOST_LANG`). `--ssh-key PATH` (`GHOST_SSH_KEY`) sets `ssh -i PATH -o IdentitiesOnly=yes` for ghost's git and the inner agent. With no key, ghost does not force an identity (it does not default to `~/.ssh/id_rsa`). `-y` / `--yes` accepts the continue prompt. `--max-iterations` (default 20) caps agent turns for this process. `--trust-comments collaborators|contributors|all` (default `collaborators`) chooses who, besides the PR author and the repo owner, may wake code mode.

A PR URL with no flag asks review or code. `--review` / `--code` on a repo URL is an error. `--review` on an issue URL is an error. A repository with no open issues and no open PRs prints a one-line notice and exits 0 without the picker.

![Picker](docs/screenshots/picker.png)

Ghost polls with `gh` only. Each poll logs `reviews N, inline N, comments N` from `gh api` (review submissions, inline review comments, and issue comments). It starts the agent on the first real task and later updates are follow-ups in that same session. A poll with no new SHA (review) or no new trusted feedback and no new CI failure signature (code) makes no agent call. A failed agent turn does not mark that SHA or those comments done; a failed resume is retried once as a fresh session with the same context. The same CI failure on the same SHA is not sent again until the SHA or the failing-check set changes.

After `issue/N` opens (or finds) a PR, ghost asks `Continue as --code on PR #M? [Y/n]` (Japanese by default) and keeps the session that already has the issue context. Comments posted after the PR was opened are not marked seen at that handoff. The agent's own replies are ignored later; a review from the same GitHub user is not. The PR body is given a `Closes #N` line when ghost is editing it and the line is missing. Issue branches look like `fix/12-crash-on-empty-config`, never `ghost/issue-N`.

The agent stops when the PR is merge-ready, merged, or closed. Ghost never merges, and the agent's `PATH` starts with a `gh` wrapper that refuses `gh pr merge`, `gh repo delete`, and similar destructive commands.

![Same session, code mode](docs/screenshots/code.png)

Continuation flags:

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>` with further prompts as JSON on stdin. If that process exits: the same flags with `--resume <uuid>` (not `--session-id`).
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`, then `codex exec resume <thread_id>` with the same flags.
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`, then `--resume <uuid>` without `--session-id`.

Prompts tell the agent to use `gh` for every GitHub action, to reply in the selected language, and not to merge.

![Continue as --code](docs/screenshots/confirm.png)

## Work directory

Clones live at `~/.ghost/work/OWNER__REPO` (`GHOST_HOME` overrides `~/.ghost`). ghost clones with `gh repo clone` and keeps the checkout after exit. Each PR or issue gets its own git worktree under `~/.ghost/work/OWNER__REPO.wt/` so two ghost processes on the same repo do not share a checkout, and a lock file stops a second process from using the same slot. Clone, fetch, and the first checkout happen before the TUI, on the normal terminal. If `origin` already matches, it fetches and stashes a dirty tree (`autostash`). A broken checkout is moved aside and cloned again. The agent's cwd is that worktree, not your shell's cwd.

## Troubleshooting

An `Enter passphrase for key` prompt is answered before the TUI starts. Later `git`/`gh` commands ghost runs itself, and the agent's `git push`, do not prompt: ssh is in `BatchMode` and `GIT_TERMINAL_PROMPT=0`. If a fetch or push fails because the key is locked, the TUI shows the error. Unlock the key with `ssh-add`, pass `--ssh-key`, or switch the remote to HTTPS with `gh config set git_protocol https`, then re-run ghost. Commands time out instead of hanging, and GitHub rate limits are retried with backoff.

Last reviewed SHA, seen comment ids, handled CI signature, and the session id are stored under `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`. A panic or SIGTERM restores the terminal and signals the agent process group.

Merge-ready means open (not a draft), checks green or absent, at least one approval, no outstanding changes requested, and mergeable. A later `COMMENTED` review does not clear an earlier approval from the same reviewer.

`q` quits. `i` sends a message into the same session. `ctrl-c` interrupts the turn and leaves the session id in place. `ghost --demo picker|review|code|confirm` draws the screens above with scripted data.
