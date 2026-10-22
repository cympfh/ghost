# ghost

[English](README.md) | [日本語](README.ja.md)

ghost keeps a local agent fixing a pull request's CI failures and review comments, and when it is mergeable, asks you, and ghost merges.

![Reviewing a pull request](docs/screenshots/review.png)

## Requirements

You need the [GitHub CLI](https://cli.github.com/) (`gh auth login`) and one of `codex`, `claude`, or `grok` on `PATH`.

```sh
cargo install --path .
```

If `gh` is missing, not logged in, or cannot read the repository, ghost explains the fix and exits.

## Usage

```sh
ghost github.com/OWNER/REPO                  # pick an open pull request or issue
ghost github.com/OWNER/REPO/pr/N --review    # review
ghost github.com/OWNER/REPO/pr/N --code      # fix CI failures and review comments
ghost github.com/OWNER/REPO/issue/N          # implement, open a pull request, then keep fixing
ghost github.com/OWNER/REPO/issue/N --yes    # same, accepting the continue prompt
```

A pull request URL with no flag asks whether to review or to fix.

A repository with nothing open prints one line and exits.

A repository URL with `--review` or `--code` is an error. `--review` on an issue URL is an error.

| Short | Long | Default | Description |
| --- | --- | --- | --- |
| `-r` | `--review` | | Review the pull request. Cannot be combined with `-c`. |
| `-c` | `--code` | | Fix failing CI and new review comments. Cannot be combined with `-r`. |
| `-i` | `--interval` | `20s` | How often to look (`30s`, `2m`, `1h30m`, or a number of seconds). |
| `-a` | `--agent` | first on `PATH` | `codex`, `claude`, or `grok`. |
| `-k` | `--ssh-key` | ssh config, or `GHOST_SSH_KEY` | Private key passed to ssh. |
| `-l` | `--lang` | `ja` (`GHOST_LANG` if the flag is omitted) | `ja` or `en`. |
| `-y` | `--yes` | | Accept the continue prompt and the review-exit prompt. |
| | `--merge-yes` | | Merge without asking. Only with `--code` or an issue. |
| | `--merge-method` | `squash` | `merge`, `squash`, or `rebase`. |
| `-n` | `--max-iterations` | `20` | Stop after this many agent turns. |
| `-t` | `--trust-comments` | `all` | Who may wake `--code`: `all`, `collaborators`, or `contributors`. |
| `-d` | `--demo` | | Draw a sample screen: `picker`, `review`, `code`, `confirm`, or `preflight`. |
| `-h` | `--help` | | Help. |
| `-V` | `--version` | | Version. |

## Screen and keys

![Picker](docs/screenshots/picker.png)

![Review](docs/screenshots/review.png)

![Fixing in the same session](docs/screenshots/code.png)

![Continue after opening a pull request](docs/screenshots/confirm.png)

`q` quits.

`i` sends a line to the agent. If a turn is running, ghost sends the line when that turn ends.

`ctrl-c` interrupts the current turn and leaves the session in place.

`y` accepts the dialog.

`n` cancels a merge, declines continuing into fixes, and exits on the review-finished dialog.

`c` closes the merge dialog and the review-finished dialog, and stays in the session.

`m`, Tab, and the left and right arrows cycle the merge method through squash, merge, and rebase.

Enter cancels a merge, exits on the review-finished dialog, and accepts continuing into fixes.

`ghost --demo picker` draws these screens with sample data. The other scenes are `review`, `code`, and `confirm`.

## Agent permissions

The agent runs with its permission prompts turned off (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, or `--yolo`). Use `--review` only on a repository you trust with that agent's credentials.

The `gh` and `git` commands placed in front of the agent only stop accidents, such as an unintended merge. To block merges for real, use a token that does not have permission to merge.

## Troubleshooting

An ssh passphrase is asked before the screen starts. After that, ssh runs without another prompt.

If the key is locked, unlock it with `ssh-add`, pass `--ssh-key`, or switch the remote to HTTPS with `gh config set git_protocol https`.

Clones stay at `~/.ghost/work/OWNER__REPO`. Set `GHOST_HOME` to replace `~/.ghost`.

The last review, seen comments, and the session id are stored in `$GHOST_STATE_DIR`, or `$XDG_STATE_HOME/ghost`, or `~/.local/state/ghost`.

A pull request is mergeable when it is open, not a draft, free of conflicts, has no outstanding change requests, and every listed check has passed.

When no checks are listed, CI is not part of that decision.

ghost asks before it merges. With `--merge-yes`, it merges without asking.

How ghost checks, merges, and lays out its working copies is in [docs/internals.md](docs/internals.md).
