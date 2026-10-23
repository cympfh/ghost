<p align="center"><img src="docs/logo.svg" width="120" alt="ghost"></p>

# ghost

[English](README.md) | [日本語](README.ja.md)

ghost has a local coding agent review a pull request, implement an issue, or fix CI failures and review comments. When the pull request is mergeable, it confirms with you and then merges itself.

## Requirements

You need the [GitHub CLI](https://cli.github.com/) (`gh auth login`) and one of `codex`, `claude`, or `grok` on `PATH`.

```sh
cargo install --path .
```

If `gh` is missing, not logged in, or cannot read the repository, ghost explains the fix and exits.

## Usage

```sh
ghost github.com/OWNER/REPO                  # pick an open pull request or issue
ghost github.com/OWNER/REPO/pull/N --review  # review
ghost github.com/OWNER/REPO/pull/N --code    # fix CI failures and review comments
ghost github.com/OWNER/REPO/pulls --review   # pick an open pull request to review
ghost github.com/OWNER/REPO/pulls --code     # pick an open pull request to fix
ghost github.com/OWNER/REPO/issues/N         # implement, open a pull request, then keep fixing
ghost github.com/OWNER/REPO/issues/N --yes   # same, accepting the continue prompt
ghost github.com/OWNER/REPO/issues --auto    # oldest open issue, then the next
```

A pull request URL with no flag asks whether to review or to fix.

A repository with nothing open prints one line and exits.

A repository URL with `--review` or `--code` is an error. `/pulls` without one of those is an error. `--review` on an issue URL is an error. `--auto` on a repository, pull request, numbered issue, or `/pulls` is an error.

`github.com/OWNER/REPO/pulls` lists open pull requests and needs `--review` or `--code`.

`github.com/OWNER/REPO/issues` lists only open issues.

`--auto` skips that list. Each time ghost checks GitHub, it takes the oldest open issue it has not handled and runs the usual issue flow: implement, open a pull request, then keep up with review. `--trust-issues` chooses which authors it runs. The default is an OWNER, MEMBER, or COLLABORATOR. When nothing is left, it waits for the next check instead of exiting. `q` or Ctrl-C ends the loop.

An issue that already has an open pull request is skipped for that round. Issues you file after `--auto` starts are picked up on a later check. An issue the agent creates with `gh issue create` in this repository is listed in `OWNER__REPO/created-issues` and skipped.

Without `--yes`, a session that is not a terminal declines the prompt that would continue into code and takes the next issue. It does not reach the merge confirm. On a terminal, `y` merges and continues, `c` stays on this pull request, and `n` or Enter leaves it unmerged and takes the next issue. `--auto --yes --merge-yes` opens the pull request, handles review, merges, and then takes the next issue.

A pull request is mergeable when it is open, not a draft, free of conflicts, has no outstanding change requests, and every listed check has passed. When no checks are listed, CI is not part of that decision. ghost confirms with you before merging, and `--merge-yes` merges without that confirmation.

| Short | Long | Default | Description |
| --- | --- | --- | --- |
| `-r` | `--review` | | Review the pull request. Cannot be combined with `-c`. |
| `-c` | `--code` | | Fix failing CI and new review comments. Cannot be combined with `-r`. |
| `-i` | `--interval` | `20s` | How often to check GitHub (`30s`, `2m`, `1h30m`, or a number of seconds). |
| `-a` | `--agent` | first on `PATH` | `codex`, `claude`, or `grok`. |
| `-k` | `--ssh-key` | ssh config, or `GHOST_SSH_KEY` | Private key passed to ssh. |
| `-l` | `--lang` | `ja` (`GHOST_LANG` if the flag is omitted) | `ja` or `en`. |
| `-y` | `--yes` | | Accept the continue prompt and the review-exit prompt. Does not merge. |
| | `--merge-yes` | | Merge without confirming. Only with `--code`, an issue, or `--auto`. |
| | `--auto` | | With `/issues`, take the oldest open issue on each check. No picker. |
| | `--merge-method` | `squash` | `merge`, `squash`, or `rebase`. |
| `-n` | `--max-iterations` | `20` | Stop after this many agent turns. |
| `-t` | `--trust-comments` | `all` | Who may wake `--code`: `all`, `collaborators`, or `contributors`. |
| | `--trust-issues` | `collaborators` | Whose issues `--auto` runs: `all`, `collaborators`, or `contributors`. |
| `-d` | `--demo` | | Draw a sample screen: `picker`, `review`, `code`, `confirm`, or `preflight`. |
| `-h` | `--help` | | Help. |
| `-V` | `--version` | | Version. |

## Screen and keys

![Picker](docs/screenshots/picker.png)

![Review](docs/screenshots/review.png)

![Fixing in the same session](docs/screenshots/code.png)

![Continue after opening a pull request](docs/screenshots/confirm.png)

- `q` quits ghost.
- `i` sends one line to the agent from the log screen. If a turn is running, the line is delivered when that turn ends.
- `ctrl-c` interrupts the current turn and leaves the session. On a confirm screen it does the same as `n`.

| Screen | `y` | `n` / Esc | Enter | `c` | `m` |
| --- | --- | --- | --- | --- | --- |
| Merge confirm | Merge | Exit ghost without merging | Exit ghost without merging | Stay in the session | Cycle the method |
| Merge confirm (`--auto`) | Merge, then take the next issue | Leave it unmerged and take the next issue | Leave it unmerged and take the next issue | Stay on this pull request | Cycle the method |
| Review-end confirm | Exit ghost | Exit ghost | Exit ghost | Stay in the session | — |
| Issue-to-code handoff | Continue fixing | Exit ghost without continuing | Continue fixing | — | — |

On the merge confirm screen, Tab and the left and right arrows cycle the method the same way `m` does: squash, merge, rebase.

`ghost --demo picker` draws these screens with sample data. The other scenes are `review`, `code`, `confirm`, and `preflight`.

## Agent permissions

The agent runs with its permission prompts turned off (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, or `--yolo`). Use `--review` only on a repository you trust with that agent's credentials.

The `gh` and `git` commands placed in front of the agent only stop accidents, such as an unintended merge. To block merges for real, use a token that does not have permission to merge.

`--auto` runs that agent on each chosen issue without asking first. `--trust-issues` defaults to collaborators (OWNER, MEMBER, or COLLABORATOR). Text in such an issue can still tell the agent what to do, so use `--auto` only on a repository you trust. `--trust-issues all` also takes issues opened by other people. `--trust-comments` defaults to `all` and chooses who may wake `--code`, including review comments while implementing an issue and during `--code`. It does not choose issue authors.

## Troubleshooting

An ssh passphrase is asked before the screen starts. After that, ssh runs without another prompt.

If the key is locked, unlock it with `ssh-add`, pass `--ssh-key`, or switch the remote to HTTPS with `gh config set git_protocol https`.

Clones stay at `~/.ghost/work/OWNER__REPO`. Set `GHOST_HOME` to replace `~/.ghost`.

The last review, seen comments, and the session id are stored in `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`.

`--auto` stores issues it will not take again in `OWNER__REPO/auto.json` under that directory. Delete the file to take them again. A temporary GitHub error is retried after a wait, or when the pull request head changes, and stays recorded after five tries. A pull request ghost opened (the issue's branch, or one the agent created with `gh pr create`) that was later closed stays recorded so ghost does not redo that issue. A closed pull request someone else opened, including one whose text says `Closes #N`, does not.

The interval for checking GitHub, the working copies, and the merge steps are in [docs/internals.md](docs/internals.md).
