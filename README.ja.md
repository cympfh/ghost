# ghost

[English](README.md) | [日本語](README.ja.md)

ghost は、ローカルのコーディングエージェントにプルリクエストをレビューさせたり、issue から実装させたり、CI の失敗とレビューコメントを直させたりします。マージできる状態になると、確認してから自分でマージします。

## 必要なもの

[GitHub CLI](https://cli.github.com/)（`gh auth login`）と、`PATH` 上の `codex`、`claude`、`grok` のいずれかが必要です。

```sh
cargo install --path .
```

`gh` が無い、ログインしていない、またはリポジトリを読めないときは、直し方を出して終了します。

## 使い方

```sh
ghost github.com/OWNER/REPO                  # 開いているプルリクエストか issue を選ぶ
ghost github.com/OWNER/REPO/pr/N --review    # レビューする
ghost github.com/OWNER/REPO/pr/N --code      # CI の失敗とレビューコメントを直す
ghost github.com/OWNER/REPO/issue/N          # 実装してプルリクエストを開き、直し続ける
ghost github.com/OWNER/REPO/issue/N --yes    # 続く確認を受け入れて始める
```

フラグの無いプルリクエストの URL では、レビューするか直すかを尋ねます。

開いているプルリクエストも issue も無いときは、一行を出して終了します。

リポジトリの URL に `--review` か `--code` を付けるとエラーです。issue の URL に `--review` を付けるとエラーです。

マージできるのは、開いていて下書きではなく、衝突がなく、未解決の変更要求がなく、並んでいるチェックがすべて成功しているときです。チェックが一覧に無いときは、CI の成功を条件にしません。マージの前に確認し、`--merge-yes` のときだけ確認せずにマージします。

| 短縮 | 長い形 | 既定 | 説明 |
| --- | --- | --- | --- |
| `-r` | `--review` | | プルリクエストをレビューする。`-c` とは同時に付けられない。 |
| `-c` | `--code` | | 失敗している CI と新しいレビューコメントを直す。`-r` とは同時に付けられない。 |
| `-i` | `--interval` | `20s` | GitHub を確認する間隔。`30s`、`2m`、`1h30m`、または秒数。 |
| `-a` | `--agent` | `PATH` の最初 | `codex`、`claude`、または `grok`。 |
| `-k` | `--ssh-key` | ssh の設定、または `GHOST_SSH_KEY` | ssh に渡す秘密鍵。 |
| `-l` | `--lang` | `ja`（フラグが無いときは `GHOST_LANG`） | `ja` または `en`。 |
| `-y` | `--yes` | | 続ける確認と、レビューを終える確認を受け入れる。マージはしない。 |
| | `--merge-yes` | | 確認せずにマージする。`--code` か issue のときだけ。 |
| | `--merge-method` | `squash` | `merge`、`squash`、または `rebase`。 |
| `-n` | `--max-iterations` | `20` | エージェントに頼む回数の上限。 |
| `-t` | `--trust-comments` | `all` | 直しを始めるコメントの投稿者。`all`、`collaborators`、`contributors`。 |
| `-d` | `--demo` | | 見本の画面。`picker`、`review`、`code`、`confirm`、`preflight`。 |
| `-h` | `--help` | | ヘルプ。 |
| `-V` | `--version` | | バージョン。 |

## 画面とキー

![選択画面](docs/screenshots/picker.png)

![レビュー](docs/screenshots/review.png)

![同じセッションで直す](docs/screenshots/code.png)

![プルリクエストを開いたあとに続ける](docs/screenshots/confirm.png)

- `q` で ghost を終了します。
- `i` はログ画面からエージェントに一文を送ります。作業中なら、その作業が終わってから届きます。
- `ctrl-c` は今の作業を中断し、セッションは残します。確認の画面では `n` と同じです。

| 画面 | `y` | `n`・Esc | Enter | `c` | `m` |
| --- | --- | --- | --- | --- | --- |
| マージの確認 | マージする | マージせずに ghost を終了する | マージせずに ghost を終了する | セッションに残る | 方法を切り替える |
| レビュー終了の確認 | ghost を終了する | ghost を終了する | ghost を終了する | セッションに残る | — |
| issue から直す確認 | 直し続ける | 続けずに ghost を終了する | 直し続ける | — | — |

マージの確認では、Tab と左右のキーも `m` と同じく方法を切り替えます。順は squash、merge、rebase です。

`ghost --demo picker` は見本のデータでこれらの画面を描きます。ほかの場面は `review`、`code`、`confirm`、`preflight` です。

## エージェントの権限

エージェントは許可を求めずに動きます（`--dangerously-skip-permissions`、`--dangerously-bypass-approvals-and-sandbox`、`--yolo`）。`--review` は、その資格情報を預けてよいリポジトリだけで使ってください。

エージェントの前に置く `gh` と `git` は、意図しないマージのような事故を止めるだけです。マージを本当に禁じるには、マージ権限の無いトークンを使ってください。

## よくある問題

ssh のパスフレーズは、画面が始まる前に答えます。そのあとは、パスフレーズなしで進みます。

鍵がロックされているときは、`ssh-add` で開くか、`--ssh-key` を渡すか、`gh config set git_protocol https` で HTTPS にしてください。

クローンは `~/.ghost/work/OWNER__REPO` に残します。`GHOST_HOME` を置くと、`~/.ghost` の代わりになります。

最後のレビュー、読んだコメント、セッションの id は `$GHOST_STATE_DIR`、`$XDG_STATE_HOME/ghost`、または `~/.local/state/ghost` にあります。

GitHub を確認する間隔、作業用コピー、マージの手順は [docs/internals.md](docs/internals.md) にあります。
