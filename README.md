# ghost

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

`--interval` defaults to `5m` (`30s`, `2m`, `1h30m`, or a bare number of seconds). `--agent codex|claude|grok` overrides the default, which is the first of those on `PATH`. `--max-iterations` (default 20) caps agent turns for this process.

A PR URL with no flag asks review or code. `--review` / `--code` on a repo URL is an error. `--review` on an issue URL is an error.

![Picker](docs/screenshots/picker.png)

Ghost polls with `gh`. It starts the agent on the first real task and later updates are follow-ups in that same session. Nothing new means no agent call. After `issue/N` opens (or finds) a PR, ghost asks `Continue as --code on PR #M? [Y/n]` and keeps the session that already has the issue context. It does not start a second agent at that transition. The agent stops when the PR is merge-ready, merged, or closed. Ghost never merges.

![Same session, code mode](docs/screenshots/code.png)

Continuation flags:

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>` with further prompts as JSON on stdin. If that process exits: the same flags with `--resume <uuid>` (not `--session-id`).
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`, then `codex exec resume <thread_id>` with the same flags.
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`, then `--resume <uuid>` without `--session-id`.

Prompts tell the agent to use `gh` for every GitHub action and not to merge.

![Continue as --code](docs/screenshots/confirm.png)

## Work directory

Clones live at `~/.ghost/work/OWNER__REPO` (`GHOST_HOME` overrides `~/.ghost`). ghost clones with `gh repo clone` and keeps the checkout after exit. If `origin` already matches, it fetches, checks out the PR or `ghost/issue-N`, and stashes a dirty tree. A broken checkout is moved aside and cloned again. The agent's cwd is that directory, not your shell's cwd.

Last reviewed SHA, seen comment ids, and the session id are stored under `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`.

Merge-ready means open (not a draft), checks green or absent, at least one approval, no outstanding changes requested, and mergeable.

`q` quits. `i` sends a message into the same session. `ctrl-c` interrupts the turn and leaves the session id in place. `ghost --demo picker|review|code|confirm` draws the screens above with scripted data.
