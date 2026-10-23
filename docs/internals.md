# Internals

Behavior that the user guide does not spell out. ghost talks to GitHub only through `gh`.

A target is `github.com/OWNER/REPO`, `.../pull/N`, `.../issues/N`, or `.../issues`. `/pr/N` and `/issue/N` are rejected, and the error names `/pull/N` or `/issues/N`. The local ref `refs/remotes/origin/pr/N` is where a fetched `pull/N/head` is stored. It is not a URL ghost accepts.

## What each check logs

Each check logs `reviews N, inline N, comments N` from `gh api`: review submissions (`repos/OWNER/REPO/pulls/N/reviews`), inline comments (`pulls/N/comments`, paged), and issue comments. A code-mode check also logs `filtered 0`, or `filtered N (untrusted author)` when `--trust-comments` drops authors.

`--trust-comments` defaults to `all`: humans, Copilot, other bots, and people outside the repository. That default applies to review comments in `--code` and while implementing an issue. Copilot review bodies (`copilot-pull-request-reviewer[bot]`) and inline comments (`Copilot`, or that same bot login) stay. `collaborators` and `contributors` narrow the set. The pull request author and the repository owner always pass. The prompt tells the agent to judge each comment, fix and push when it agrees, and reply on the thread when it does not.

`--trust-issues` is separate and defaults to `collaborators` (OWNER, MEMBER, COLLABORATOR). It only chooses which issue authors `--auto` runs. `--trust-comments all` does not widen that set.

On launch, review, code, and issue mode each start the agent once, even when saved state says nothing is new. That launch starts a new session. Later checks call the agent only when something changed: a new commit in review, or new feedback or a new CI failure signature in code. A failed turn does not mark that commit or those comments done. A failed resume starts a fresh session with the same context. A resume that dies because the session is missing (`no rollout found`) or because the arguments were rejected does not count as a turn and does not wait. A prompt write that gets `EPIPE` fails that turn and does not kill ghost. Real failures back off from 20 seconds, doubling up to 10 minutes, and ghost exits 1 after five in a row.

The same CI failure on the same commit is not sent again until the commit or the set of failing checks changes. A rate-limit retry happens only when the command's exit code is not 0 and stderr says rate limit, HTTP 429, or secondary rate.

`i` always reaches the agent. A running turn queues the line and sends it when the turn ends. Comments the agent posts during that queued follow-up are not treated as new work. The log says `→ エージェントに送信`, or a queued line that includes that phrase.

## Issue branches and closing lines

The branch name comes from the issue title and labels, in the form `fix/12-crash-on-empty-config`: a type (`feat`, `fix`, `docs`, `refactor`, `chore`, `test`), the issue number, and a short kebab slug. When ghost edits the pull request body, it adds `Closes #N` if that line is missing. After the pull request exists, ghost asks whether to continue fixing. Comments posted after the pull request was opened stay unseen at that handoff. The agent's own replies are later ignored. A review from the same GitHub user is not.

## Untrusted pull request text

Review prompts wrap the title, diff, and comment bodies:

```text
<<<UNTRUSTED_TITLE>>>
<<<UNTRUSTED_DIFF>>>
<<<UNTRUSTED_COMMENT>>>
```

Each block ends with `<<<END_UNTRUSTED_…>>>`. Copies of those markers inside the text, including different case and whitespace, are replaced before the real markers are added. The wrapping is not a security boundary. A hostile diff can still confuse the agent.

A review turn that ends with `GHOST_VERDICT: approve` asks whether to exit. `c` keeps watching and asks again on the next approve. On your own pull request, if GitHub rejects approve, the agent posts `gh pr review --comment` with `LGTM / 承認相当` and still ends with `GHOST_VERDICT: approve`.

## When ghost merges

Mergeable means open, not a draft, `mergeStateStatus` CLEAN, not conflicting, and no outstanding changes requested. If `gh pr checks` lists checks, every one must have passed. A pending check waits. An empty list means there is no CI. Approvals are not required. A later `COMMENTED` review does not clear an earlier approval from the same reviewer.

ghost offers merge only after the same commit has been mergeable on two checks at least one interval apart. The refetch at the end of the opening turn is not one of those checks. `--merge-yes` waits for the second check too. `y` re-reads the commit and runs `gh pr merge` with exactly one of `--merge`, `--squash`, or `--rebase`, plus `--match-head-commit`. The method on screen starts as `--merge-method`, or squash when the flag is omitted. `c` asks again only after the commit, a review or comment, or an agent turn changes.

`--merge-yes` merges without asking and exits 1 if `gh pr merge` fails, instead of trying again on every check. If the repository rejects the method, the log says so and points at `--merge-method` or the `m` key. ghost does not pick another method. `-y` answers the continue and review-exit prompts only.

When stdin or stdout is not a terminal, ghost does not open `/dev/tty`. Opening that device with no controlling terminal fails with ENXIO (os error 6). Confirms then take the non-interactive answers: review exits, an issue does not continue into code without `-y`, and a merge happens only with `--merge-yes`.

The agent's `PATH` starts with a `gh` wrapper that refuses `gh pr merge`, `gh repo delete`, `gh api` DELETE, and merge endpoints such as `mergePullRequest`. The `git` wrapper skips global options (`-C`, `-c`, `--git-dir`, `--work-tree`, `--namespace`, `--no-pager`, `--bare`, and the rest) and then sends `git push` to the planned remote and refspec, or refuses it in review and when a fork is not pushable. An alias or an absolute `/usr/bin/gh` or `/usr/bin/git` still runs. Neither wrapper is a security boundary.

A successful `gh issue create` for this repository appends the issue number to `OWNER__REPO/created-issues`, and `--auto` skips that number. `-R` and `--repo` must be omitted or name this repository; another repository is not recorded. `gh issue create --web` often prints no issue number, and `gh api` (including a GraphQL `createIssue`) is not watched, so those issues are not skipped. A successful `gh pr create` for this repository is recorded in `OWNER__REPO/created-prs`. Together with the issue branch name, that is how a closed pull request counts as ghost's.

A GitHub failure is transient only when `gh` says so on stderr (or the command times out). The recorded reason is not scanned, so a branch such as `fix/8-fix-timeout` or issue `#502` does not retry by itself. A transient failure is retried after a wait, or when the pull request head changes, and becomes permanent after five attempts. Both paths share that cap. The head compared on the next check is the one seen before the attempt, so a push during the attempt looks like a new head and still counts.

## Agent commands

Prompts tell the agent to use `gh` for GitHub, to reply in the selected language, and to leave merging to ghost.

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>`. Later prompts are JSON on stdin. If the process exits, the same flags use `--resume <uuid>`.
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`, then `codex --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check resume <thread_id>`. Resume has no `-C` and no `-s`.
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`, then `--resume <uuid>` without `--session-id`.

Prompt files are `~/.ghost/prompts/pid<pid>-<slot>/`, one directory per process and per mode.

Secondary text is ANSI gray, not bright black. A non-empty `NO_COLOR` turns color off. Unset or empty `NO_COLOR` leaves color on.

## Working copies

`gh repo clone` writes `~/.ghost/work/OWNER__REPO` and leaves it after exit. Review and code use separate directories, `pr-N-review` and `pr-N-code`, each with its own worktree, lock, and state file under `~/.ghost/work/OWNER__REPO.wt/`. The local branch name is that directory name. It does not track `origin`.

ghost fetches `origin pull/N/head` into `refs/remotes/origin/pr/N` and checks that commit out. It does not fetch `refs/heads/<headRef>` into `origin`, so a fork whose branch is also named `main` leaves `origin/main` where it is. A same-repository push is `git push origin HEAD:<headRef>`. A fork is pushed to the head repository only when `maintainerCanModify` is set. If `gh` omits the head URL, ghost uses `git@github.com:owner/name.git` so `--ssh-key` applies. Otherwise `git push` is refused.

A failed lock stops that mode. The working directory for an issue is `issue-N`. The git branch is the slug from the issue title, such as `fix/12-crash-on-empty-config`. Checking that branch out can detach the main clone. A dirty tree is stashed, and the log says `ghost: detach main clone off <branch>`. Clone, fetch, and the first checkout run on the normal terminal before the screen, so ssh can ask for a passphrase. A broken checkout is moved aside and cloned again.

If clone, fetch, or worktree checkout fails, ghost stays up. The screen shows the step in Japanese (why, the command, the exit code, trimmed stderr, and the desired state) and the agent is asked to reach that state itself. When the worktree directory is missing, the agent starts in `~/.ghost/work` with that failed step in the prompt. ghost stops only when `gh` is missing, `gh` is not logged in, or no coding agent is on `PATH`.

SIGTERM and SIGHUP restore the terminal settings saved at startup, leave the alternate screen, and signal the agent process group. A panic in the worker is logged and the screen exits.
