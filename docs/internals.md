# Internals

Details behind the short READMEs. ghost talks to GitHub only through `gh`.

A target is `github.com/OWNER/REPO`, `.../pull/N`, `.../pulls`, `.../issues/N`, or `.../issues`. `/pulls` lists open pull requests and requires `--code` or `--review` before any network call; the error is `/pulls requires --code or --review`. `/pulls/N` is rejected, and the error names `/pull/N`. `/pr/N` and `/issue/N` are rejected, and the error names `/pull/N` or `/issues/N`. The local ref `refs/remotes/origin/pr/N` is where a fetched `pull/N/head` is stored. It is not a URL ghost accepts.

You need `gh` (`gh auth login`) and one of `codex`, `claude`, `grok`, or `kimi` on `PATH`. Install with `cargo install --git https://github.com/cympfh/ghost`. If `gh` is missing, not logged in, or cannot read the repository, ghost explains the fix and exits. It also stops when no coding agent is on `PATH`.

## Flags

| Short | Long | Default | Description |
| --- | --- | --- | --- |
| `-r` | `--review` | | Review the pull request. Cannot be combined with `-c`. |
| `-c` | `--code` | | Fix failing CI and new review comments. Cannot be combined with `-r`. |
| `-i` | `--interval` | `20s` | How often to check GitHub (`30s`, `2m`, `1h30m`, or a number of seconds). |
| `-a` | `--agent` | first on `PATH` | `codex`, `claude`, `grok`, or `kimi`. |
| `-k` | `--ssh-key` | ssh config, or `GHOST_SSH_KEY` | Private key passed to ssh. |
| `-l` | `--lang` | `ja` (`GHOST_LANG` if the flag is omitted) | `ja` or `en`. |
| | `--merge-yes` | | Merge without confirming. Only with `--code`, an issue, or `--auto`. Conflicts with `--merge-no`. |
| | `--merge-no` | | Exit successfully without merging or confirming. Only with `--code`, an issue, or `--auto`. Conflicts with `--merge-yes`. |
| `-s` | `--skip-approval` | | Do not require approval for merge readiness. CI and outstanding fix requests are still required. Only with `--code`, an issue, or `--auto`. |
| | `--auto` | | With `/issues`, take the oldest open issue on each check. No picker. |
| | `--merge-method` | `squash` | `merge`, `squash`, or `rebase`. |
| `-n` | `--max-iterations` | `20` | Stop after this many agent turns. |
| `-t` | `--trust-comments` | `all` | Who may wake `--code`: `all`, `collaborators`, or `contributors`. |
| | `--trust-issues` | `collaborators` | Whose issues `--auto` runs: `all`, `collaborators`, or `contributors`. |
| `-d` | `--demo` | | Draw a sample screen: `picker`, `review`, `code`, `confirm`, or `preflight`. |
| `-h` | `--help` | | Help. |
| `-V` | `--version` | | Version. |

A pull request URL with no flag asks whether to review or to fix. A repository with nothing open prints one line and exits. A repository URL with `--review` or `--code` is an error. `--review` on an issue URL is an error. `--auto` is only valid with `github.com/OWNER/REPO/issues`. That URL lists only open issues.

## Keys

- `q` quits ghost.
- `i` sends one line to the agent from the log screen. If a turn is running, the line is delivered when that turn ends.
- `ctrl-c` interrupts the current turn and leaves the session. On a confirm screen it does the same as `n`.

| Screen | `y` | `n` / Esc | Enter | `c` | `m` |
| --- | --- | --- | --- | --- | --- |
| Merge confirm | Merge | Exit ghost without merging | Exit ghost without merging | Stay in the session | Cycle the method |
| Merge confirm (`--auto`) | Merge, then take the next issue | Leave it unmerged and take the next issue | Leave it unmerged and take the next issue | Stay on this pull request | Cycle the method |

On the merge confirm screen, Tab and the left and right arrows cycle the method the same way `m` does: squash, merge, rebase.

`ghost --demo picker` draws these screens with sample data. The other scenes are `review`, `code`, `confirm`, and `preflight`. Screenshots of those screens are in `docs/screenshots/`.

## `--auto`

`--auto` skips the issue list. Each check takes the oldest open issue it has not handled and runs the usual issue flow: implement, open a pull request, then keep up with review. When nothing is left, it waits for the next check instead of exiting. `q` or Ctrl-C ends the loop.

An issue that already has an open pull request is skipped for that round. Issues filed after `--auto` starts are picked up on a later check. Which authors it runs is `--trust-issues` (default OWNER, MEMBER, or COLLABORATOR). An issue the agent creates with `gh issue create` in this repository is listed in `OWNER__REPO/created-issues` and skipped. See below for closed pull requests ghost opened, and for which GitHub errors are retried.

After the pull request exists, ghost continues into code without asking, including in a non-terminal session. On a terminal, `y` merges and continues, `c` stays on this pull request, and `n` or Enter leaves it unmerged and takes the next issue. `--auto --merge-yes` opens the pull request, handles review, merges, and then takes the next issue. `--auto --merge-no` leaves it unmerged and takes the next issue without asking.

Issues `--auto` will not take again are stored in `OWNER__REPO/auto.json` under the state directory below. Delete that file to take them again.

## Agent permissions

The agent runs with its permission prompts turned off (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, or `--yolo`). Use `--review` only on a repository you trust with that agent's credentials.

The `gh` and `git` commands placed in front of the agent only stop selected accidents. The agent can use `gh pr merge` and merge endpoints. To block merges, use a token that does not have permission to merge.

`--auto` runs that agent on each chosen issue without asking first. Text in such an issue can still tell the agent what to do, so use `--auto` only on a repository you trust. `--trust-issues all` also takes issues opened by other people. `--trust-comments` defaults to `all` and chooses who may wake `--code`, including review comments while implementing an issue and during `--code`. It does not choose issue authors.

## Paths

An ssh passphrase is asked before the screen starts. After that, ssh runs without another prompt. If the key is locked, unlock it with `ssh-add`, pass `--ssh-key`, or switch the remote to HTTPS with `gh config set git_protocol https`.

Clones stay at `~/.ghost/work/OWNER__REPO`. Set `GHOST_HOME` to replace `~/.ghost`.

The last review, seen comments, and the session id are stored in `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`.

## What each check logs

Each check logs `reviews N, inline N, comments N` from `gh api`: review submissions (`repos/OWNER/REPO/pulls/N/reviews`), inline comments (`pulls/N/comments`, paged), and issue comments. A code-mode check also logs `filtered 0`, or `filtered N (untrusted author)` when `--trust-comments` drops authors.

`--trust-comments` defaults to `all`: humans, Copilot, other bots, and people outside the repository. That default applies to review comments in `--code` and while implementing an issue. Copilot review bodies (`copilot-pull-request-reviewer[bot]`) and inline comments (`Copilot`, or that same bot login) stay. `collaborators` and `contributors` narrow the set. The pull request author and the repository owner always pass. The prompt tells the agent to judge each comment, fix and push when it agrees, and then reply that it was addressed. When it intentionally does not make the requested change, it replies on the thread with the reason. It does not reply to approvals or approval-equivalent comments and instead proceeds directly to the merge-readiness decision when no fix request remains.

`--trust-issues` is separate and defaults to `collaborators` (OWNER, MEMBER, COLLABORATOR). It only chooses which issue authors `--auto` runs. `--trust-comments all` does not widen that set.

On launch, review, code, and issue mode each start the agent once, even when saved state says nothing is new. That launch starts a new session. Later checks call the agent only when something changed: a new commit in review, or new feedback or a new CI failure signature in code. A failed turn does not mark that commit or those comments done. A failed resume starts a fresh session with the same context. A resume that dies because the session is missing (`no rollout found`) or because the arguments were rejected does not count as a turn and does not wait. A prompt write that gets `EPIPE` fails that turn and does not kill ghost. Real failures back off from 20 seconds, doubling up to 10 minutes, and ghost exits 1 after five in a row.

The same CI failure on the same commit is not sent again until the commit or the set of failing checks changes. A rate-limit retry happens only when the command's exit code is not 0 and stderr says rate limit, HTTP 429, or secondary rate.

`i` always reaches the agent. A running turn queues the line and sends it when the turn ends. Comments the agent posts during that queued follow-up are not treated as new work. The log says `→ エージェントに送信`, or a queued line that includes that phrase.

## Issue branches and closing lines

The branch name comes from the issue title and labels, in the form `fix/12-crash-on-empty-config`: a type (`feat`, `fix`, `docs`, `refactor`, `chore`, `test`), the issue number, and a short kebab slug. When ghost edits the pull request body, it adds `Closes #N` if that line is missing. After the pull request exists, ghost continues fixing without asking. When the current issue turn created the pull request, the first code follow-up tells the agent that it just created that PR. Comments posted after the pull request was opened stay unseen at that handoff. The agent's own replies are later ignored. A review from the same GitHub user is not.

## Untrusted pull request text

Review prompts wrap the title, diff, and comment bodies:

```text
<<<UNTRUSTED_TITLE>>>
<<<UNTRUSTED_DIFF>>>
<<<UNTRUSTED_COMMENT>>>
```

Each block ends with `<<<END_UNTRUSTED_…>>>`. Copies of those markers inside the text, including different case and whitespace, are replaced before the real markers are added. The wrapping is not a security boundary. A hostile diff can still confuse the agent.

A review turn that ends with `GHOST_VERDICT: approve` exits automatically. On your own pull request, if GitHub rejects approve, the agent posts `gh pr review --comment` with `LGTM / 承認相当` and still ends with `GHOST_VERDICT: approve`.

## When ghost merges

The coding agent judges CI and reviews and ends each coding turn with `GHOST_STATUS: merge_ready` or `GHOST_STATUS: needs_work`. An empty `gh pr checks` list means CI is not configured and is acceptable; otherwise every check on the current head must pass. Review acceptance can come from an approval or a comment the agent judges to be an approval equivalent. Outstanding fix requests mean `needs_work`.

With `-s` / `--skip-approval`, the agent treats the review condition as satisfied without an approval or approval-equivalent comment. CI requirements do not change, and outstanding changes requested or fix requests still mean `needs_work`.

ghost accepts that judgment only for the same head SHA. It separately requires the PR to be open, not a draft, `mergeStateStatus` CLEAN, and not conflicting. A missing status line counts as `needs_work`.

ghost handles merge readiness only after the same commit has been mergeable on two checks at least one interval apart. The refetch at the end of the opening turn is not one of those checks. `--merge-yes` and `--merge-no` wait for the second check too. `y` re-reads the commit and runs `gh pr merge` with exactly one of `--merge`, `--squash`, or `--rebase`, plus `--match-head-commit`. The method on screen starts as `--merge-method`, or squash when the flag is omitted. `c` asks again only after the commit, a review or comment, or an agent turn changes.

`--merge-yes` merges without asking and exits 1 if `gh pr merge` fails, instead of trying again on every check. If the repository rejects the method, the log says so and points at `--merge-method` or the `m` key. ghost does not pick another method. `--merge-no` exits 0 without merging or opening the confirmation screen. The two flags cannot be combined.

When stdin or stdout is not a terminal, ghost does not open `/dev/tty`. Opening that device with no controlling terminal fails with ENXIO (os error 6). Review exits and issue mode continues into code without asking. A merge happens only with `--merge-yes`.

The agent's `PATH` starts with a `gh` wrapper that allows `gh pr merge` and merge endpoints but refuses `gh repo delete` and `gh api` DELETE. The `git` wrapper skips global options (`-C`, `-c`, `--git-dir`, `--work-tree`, `--namespace`, `--no-pager`, `--bare`, and the rest) and then sends `git push` to the planned remote and refspec, or refuses it in review and when a fork is not pushable. An alias or an absolute `/usr/bin/gh` or `/usr/bin/git` still runs. Neither wrapper is a security boundary.

A successful `gh issue create` for this repository appends the issue number to `OWNER__REPO/created-issues`, and `--auto` skips that number. `-R` and `--repo` must be omitted or name this repository; another repository is not recorded. `gh issue create --web` often prints no issue number, and `gh api` (including a GraphQL `createIssue`) is not watched, so those issues are not skipped. A successful `gh pr create` for this repository is recorded in `OWNER__REPO/created-prs`. Together with the issue branch name, that is how a closed pull request counts as ghost's.

A GitHub failure is transient only when `gh` says so on stderr (or the command times out). The recorded reason is not scanned, so a branch such as `fix/8-fix-timeout` or issue `#502` does not retry by itself. A transient failure is retried after a wait, or when the pull request head changes, and becomes permanent after five attempts. Both paths share that cap. The head compared on the next check is the one seen before the attempt, so a push during the attempt looks like a new head and still counts.

## Agent commands

Prompts tell the agent to use `gh` for GitHub, to reply in the selected language, and to report merge readiness with `GHOST_STATUS`. The agent may also run `gh pr merge`; ghost detects the merged PR on a later check.

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>`. Later prompts are JSON on stdin. If the process exits, the same flags use `--resume <uuid>`.
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`, then `codex --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check resume <thread_id>`. Resume has no `-C` and no `-s`.
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`, then `--resume <uuid>` without `--session-id`.
- Kimi: `kimi --output-format stream-json --prompt <instruction>`, then `kimi --session <id> --output-format stream-json --prompt <instruction>`. The short instruction tells Kimi to read the complete task from the prompt file, keeping large prompts out of argv. Kimi normally reports its session id in the final `session.resume_hint` event. If a failed or interrupted turn emits no hint, ghost recovers the session created after launch with `kimi session list --cwd <dir> --json`. Print mode uses automatic permissions, so ghost does not pass `--yolo`.

Prompt files are `~/.ghost/prompts/pid<pid>-<slot>/`, one directory per process and per mode.

Secondary text is ANSI gray, not bright black. A non-empty `NO_COLOR` turns color off. Unset or empty `NO_COLOR` leaves color on.

## Working copies

`gh repo clone` writes `~/.ghost/work/OWNER__REPO` and leaves it after exit. Review and code use separate directories, `pr-N-review` and `pr-N-code`, each with its own worktree, lock, and state file under `~/.ghost/work/OWNER__REPO.wt/`. The local branch name is that directory name. It does not track `origin`.

ghost fetches `origin pull/N/head` into `refs/remotes/origin/pr/N` and checks that commit out. It does not fetch `refs/heads/<headRef>` into `origin`, so a fork whose branch is also named `main` leaves `origin/main` where it is. A same-repository push is `git push origin HEAD:<headRef>`. A fork is pushed to the head repository only when `maintainerCanModify` is set. If `gh` omits the head URL, ghost uses `git@github.com:owner/name.git` so `--ssh-key` applies. Otherwise `git push` is refused.

A failed lock stops that mode. The working directory for an issue is `issue-N`. The git branch is the slug from the issue title, such as `fix/12-crash-on-empty-config`. Checking that branch out can detach the main clone. A dirty tree is stashed, and the log says `ghost: detach main clone off <branch>`. Clone, fetch, and the first checkout run on the normal terminal before the screen, so ssh can ask for a passphrase. A broken checkout is moved aside and cloned again.

If clone, fetch, or worktree checkout fails, ghost stays up. The screen shows the step in Japanese (why, the command, the exit code, trimmed stderr, and the desired state) and the agent is asked to reach that state itself. When the worktree directory is missing, the agent starts in `~/.ghost/work` with that failed step in the prompt. ghost stops only when `gh` is missing, `gh` is not logged in, or no coding agent is on `PATH`.

SIGTERM and SIGHUP restore the terminal settings saved at startup, leave the alternate screen, and signal the agent process group. A panic in the worker is logged and the screen exits.
