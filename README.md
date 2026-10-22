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
ghost github.com/OWNER/REPO/pr/N --code      # fix failing CI checks and new review comments
ghost github.com/OWNER/REPO/issue/N          # implement, open a PR, then continue as --code
ghost github.com/OWNER/REPO/issue/N --yes    # same, accepting the continue prompt
```

| Short | Long | Default | |
| --- | --- | --- | --- |
| `-r` | `--review` | | Review when the head SHA changes. Conflicts with `-c`. |
| `-c` | `--code` | | Fix failing CI checks (`gh pr checks`) and new review comments. Conflicts with `-r`. |
| `-i` | `--interval` | `20s` | Poll interval (`30s`, `2m`, `1h30m`, or a bare number of seconds). |
| `-a` | `--agent` | first on `PATH` | `codex`, `claude`, or `grok`. |
| `-k` | `--ssh-key` | ssh config, or `GHOST_SSH_KEY` | `ssh -i PATH -o IdentitiesOnly=yes`. No key means no forced identity (`~/.ssh/id_rsa` is not assumed). |
| `-l` | `--lang` | `ja` (`GHOST_LANG` if the flag is omitted) | `ja` or `en`. |
| `-y` | `--yes` | | Accept the issue→code prompt and the review exit prompt. Does not merge. |
| | `--merge-yes` | | Merge when `--code` or issue mode is merge-ready, after a re-check. No short flag. |
| | `--merge-method` | `squash` | `merge`, `squash`, or `rebase`. ghost always passes exactly one of `--merge`, `--squash`, or `--rebase` to `gh pr merge`. |
| `-n` | `--max-iterations` | `20` | Cap agent turns for this process. |
| `-t` | `--trust-comments` | `all` | Who, besides the PR author and repo owner, may wake code mode. Default: every author (humans, Copilot, other bots, outsiders). `collaborators` or `contributors` still restricts. |
| `-d` | `--demo` | | `picker`, `review`, `code`, `confirm`, or `preflight`. |
| `-h` | `--help` | | Help. |
| `-V` | `--version` | | Version. |

A PR URL with no flag asks review or code. `--review` / `--code` on a repo URL is an error. `--review` on an issue URL is an error. A repository with no open issues and no open PRs prints a one-line notice and exits 0 without the picker.

![Picker](docs/screenshots/picker.png)

Ghost polls with `gh` only. Each poll logs `reviews N, inline N, comments N` from `gh api` (review submissions, inline review comments, and issue comments). Each code poll also logs `filtered 0`, or `filtered N (untrusted author)` when `-t` drops authors. Copilot review bodies (`copilot-pull-request-reviewer[bot]`) and inline comments (`Copilot`, or the same bot login) are kept under the default `all`. On launch, `--review`, `--code`, and issue mode each start the agent once for a situation check, even when saved state says nothing is new. Later polls skip the agent when nothing changed: the same head SHA in review, or no new feedback and no new CI failure signature in code. A failed agent turn does not mark that SHA or those comments done; a failed resume is retried once as a fresh session with the same context. Repeated agent failures wait longer each time and ghost stops after five in a row. The same CI failure on the same SHA is not sent again until the SHA or the failing-check set changes.

Typing `i` always reaches the agent. A running turn queues the line and sends it when the turn finishes. If no session exists, or the previous one died, ghost starts or resumes one and sends the line. The log shows `→ エージェントに送信`, or a queued status that includes that phrase. The line is not dropped.

After `issue/N` opens (or finds) a PR, ghost asks `Continue as --code on PR #M?` (Japanese by default). Enter is yes. The footer is `Y(Enter) yes / n no` (`Y(Enter) 続ける / n やめる` in Japanese). ghost keeps the session that already has the issue context. Comments posted after the PR was opened are not marked seen at that handoff. The agent's own replies are ignored later; a review from the same GitHub user is not. The PR body is given a `Closes #N` line when ghost is editing it and the line is missing. Issue branches look like `fix/12-crash-on-empty-config`, never `ghost/issue-N`.

Review never merges. When the review agent ends a turn with `GHOST_VERDICT: approve` (or, with no verdict line, a self-review comment that says `LGTM / 承認相当`), ghost asks `PR #N は問題なしと判断しました。終了しますか？`. The dialog footer is `Y(Enter) exit / c continue` (`Y(Enter) 終了 / c 続ける` in Japanese). `y`, Enter, Esc, and Ctrl-C exit. `c` keeps watching: a new commit is reviewed again, and the next approve asks again. A non-TTY, or `-y`, answers `y`. `request_changes` and `comment` keep polling. If GitHub rejects approving your own pull request, the agent posts `gh pr review --comment` with `LGTM / 承認相当` and still ends with `GHOST_VERDICT: approve`.

Code mode, when ghost's own `gh` re-check says the PR is merge-ready, asks `PR #N を merge しますか？ 方法: squash` (English: `Merge PR #N? method: squash`). The dialog footer is `y merge / N(Enter) やめる / c 続ける / m 方法: squash` (English: `y merge / N(Enter) cancel / c continue / m method: squash`). `m`, Tab, and the left/right arrows cycle squash → merge → rebase. `y` re-checks that the head SHA is unchanged and still merge-ready, then ghost itself runs `gh pr merge` with the method on screen (the agent's `gh` wrapper is not on that path). The command always includes exactly one method flag and `--match-head-commit` with the re-checked head SHA, and logs the method (`merge method: --squash`). The initial method is `--merge-method`, or `--squash` when that flag is omitted. `n`, Enter, Esc, and Ctrl-C exit without merging. `c` keeps the session and asks again the next time the PR is merge-ready, after the poll interval. `--merge-yes` does not prompt: it merges with the flag value (default squash) and the log says it merged because of `--merge-yes`. `-y` does not merge. A non-TTY does not merge unless `--merge-yes`. `--merge-yes` is an error with `--review`, the picker, or a PR URL that does not pass `--code`. The agent's `PATH` starts with a `gh` wrapper that catches accidental `gh pr merge`, `gh repo delete`, `gh api` DELETE, and merge endpoints such as `mergePullRequest`. That wrapper is not a security boundary: `git push` to the default branch, a `gh` alias, an absolute `/usr/bin/gh`, and other API shapes still get through. For real enforcement, use a token that does not have permission to merge.

![Same session, code mode](docs/screenshots/code.png)

Continuation flags:

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>` with further prompts as JSON on stdin. If that process exits: the same flags with `--resume <uuid>` (not `--session-id`).
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`, then `codex --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check resume <thread_id>` (no `-C` and no `-s` on resume).
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`, then `--resume <uuid>` without `--session-id`.

Prompts tell the agent to use `gh` for every GitHub action, to reply in the selected language, and not to merge.

Secondary text is ANSI gray, not bright black. A non-empty `NO_COLOR` turns color off. Unset or empty `NO_COLOR` leaves color on.

![Continue as --code](docs/screenshots/confirm.png)

## Work directory

Clones live at `~/.ghost/work/OWNER__REPO` (`GHOST_HOME` overrides `~/.ghost`). ghost clones with `gh repo clone` and keeps the checkout after exit. The main clone stays on the default branch or detached HEAD. Pull-request branches, including an older `ghost/issue-1` left checked out there, are not checked out in the main clone: a dirty tree is stashed (`ghost: detach main clone off <branch>`), then the branch moves to a per-PR worktree under `~/.ghost/work/OWNER__REPO.wt/`. ghost fetches `origin pull/N/head` and adds that worktree with the head branch checked out. A lock file stops a second process from using the same slot. Clone, fetch, and the first checkout happen before the TUI, on the normal terminal. If `origin` already matches, it fetches and stashes a dirty tree (`autostash`). A broken checkout is moved aside and cloned again.

If clone, fetch, or worktree checkout fails, ghost does not exit. The TUI logs the step in Japanese (why it ran, the command, the exit code, trimmed stderr, and the desired state) and the agent is asked to reach that state its own way with `gh` and git. The working directory is the best path that still exists: the slot worktree, the clone, or `~/.ghost/work`. ghost still stops only when `gh` is missing, `gh` is not logged in, or no coding agent is on `PATH`.

## Troubleshooting

An `Enter passphrase for key` prompt is answered before the TUI starts. Later `git`/`gh` commands ghost runs itself, and the agent's `git push`, do not prompt: ssh is in `BatchMode` and `GIT_TERMINAL_PROMPT=0`. If a fetch or push fails because the key is locked, the TUI shows the error. Unlock the key with `ssh-add`, pass `--ssh-key`, or switch the remote to HTTPS with `gh config set git_protocol https`, then re-run ghost. Commands time out instead of hanging, and GitHub rate limits are retried with backoff.

Last reviewed SHA, seen comment ids, handled CI signature, and the session id are stored under `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`. A panic or SIGTERM restores the terminal and signals the agent process group.

Merge-ready means open, not a draft, `mergeStateStatus` CLEAN, not conflicting, and no outstanding changes requested. If `gh pr checks` lists checks, every one must have passed; a pending check waits. An empty check list means the PR has no CI, and CI is ignored. Approvals are not required. A later `COMMENTED` review does not clear an earlier approval from the same reviewer.

`q` quits. `i` sends a message into the same session. `ctrl-c` interrupts the turn and leaves the session id in place. `ghost --demo picker|review|code|confirm` draws the screens above with scripted data.
