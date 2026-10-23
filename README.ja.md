<p align="center"><img src="docs/logo.svg" width="120" alt="ghost"></p>

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
ghost github.com/OWNER/REPO/issues --auto    # いちばん古い issue から順に進める
```

フラグの無いプルリクエストの URL では、レビューするか直すかを尋ねます。

開いているプルリクエストも issue も無いときは、一行を出して終了します。

リポジトリの URL に `--review` か `--code` を付けるとエラーです。issue の URL に `--review` を付けるとエラーです。リポジトリ、プルリクエスト、番号付きの issue に `--auto` を付けるとエラーです。

`github.com/OWNER/REPO/issues`（`/issue` も同じ）は、開いている issue だけを一覧します。

`--auto` は一覧を出さず、GitHub を確認するたびに、まだ扱っていないいちばん古い issue を、実装からプルリクエスト、レビュー対応までのいつもの流れで進めます。

投稿者は `--trust-issues` で選び、既定は OWNER、MEMBER、COLLABORATOR です。候補が無いあいだは終了せず次の確認まで待ち、`q` と Ctrl-C でループを終えます。

開いているプルリクエストが付いている issue はその回だけ飛ばし、開始後に自分で開いた issue は次の確認で取って、エージェントがこのリポジトリで `gh issue create` した番号は `OWNER__REPO/created-issues` に残して飛ばします。

`--yes` が無いとき、端末が無ければ続ける確認をその場で断り、マージの確認には進まず次の issue へ移ります。端末では `y` がマージして次へ進み、`c` がそのプルリクエストに留まり、`n` と Enter がマージせず次の issue へ移ります。

`--auto --yes --merge-yes` は確認なしでプルリクエストを作り、レビューを直し、マージしてから次を取ります。

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
| | `--merge-yes` | | 確認せずにマージする。`--code`、issue、または `--auto` のときだけ。 |
| | `--auto` | | `/issues` で、確認のたびにいちばん古いオープンな issue を取る。一覧は出さない。 |
| | `--merge-method` | `squash` | `merge`、`squash`、または `rebase`。 |
| `-n` | `--max-iterations` | `20` | エージェントに頼む回数の上限。 |
| `-t` | `--trust-comments` | `all` | 直しを始めるコメントの投稿者。`all`、`collaborators`、`contributors`。 |
| | `--trust-issues` | `collaborators` | `--auto` が取る issue の投稿者。`all`、`collaborators`、`contributors`。 |
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
| `--auto` のマージ確認 | マージして次の issue へ進む | マージせず次の issue へ進む | マージせず次の issue へ進む | そのプルリクエストに残る | 方法を切り替える |
| レビュー終了の確認 | ghost を終了する | ghost を終了する | ghost を終了する | セッションに残る | — |
| issue から直す確認 | 直し続ける | 続けずに ghost を終了する | 直し続ける | — | — |

マージの確認では、Tab と左右のキーも `m` と同じく方法を切り替えます。順は squash、merge、rebase です。

`ghost --demo picker` は見本のデータでこれらの画面を描きます。ほかの場面は `review`、`code`、`confirm`、`preflight` です。

## エージェントの権限

エージェントは許可を求めずに動きます（`--dangerously-skip-permissions`、`--dangerously-bypass-approvals-and-sandbox`、`--yolo`）。`--review` は、その資格情報を預けてよいリポジトリだけで使ってください。

エージェントの前に置く `gh` と `git` は、意図しないマージのような事故を止めるだけです。マージを本当に禁じるには、マージ権限の無いトークンを使ってください。

`--auto` は、選んだ issue を確認なしでそのエージェントに任せます。

`--trust-issues` の既定は collaborators で、OWNER、MEMBER、COLLABORATOR が開いた issue だけを取ります。その文面もエージェントへの指示になり得るので、`--auto` は任せてよいリポジトリだけで使ってください。`--trust-issues all` にすると、外部の人が開いた issue も対象になります。

`--trust-comments` はレビューコメントの投稿者を選び、既定は `all` です。issue の投稿者の設定とは別で、番号付きの issue と `--code` のコメントにも使います。

## よくある問題

ssh のパスフレーズは、画面が始まる前に答えます。そのあとは、パスフレーズなしで進みます。

鍵がロックされているときは、`ssh-add` で開くか、`--ssh-key` を渡すか、`gh config set git_protocol https` で HTTPS にしてください。

クローンは `~/.ghost/work/OWNER__REPO` に残します。`GHOST_HOME` を置くと、`~/.ghost` の代わりになります。

最後のレビュー、読んだコメント、セッションの id は `$GHOST_STATE_DIR`、`$XDG_STATE_HOME/ghost`、または `~/.local/state/ghost` にあります。

`--auto` がやり直さない issue は、状態ディレクトリの `OWNER__REPO/auto.json` に残し、ファイルを消せばもう一度取ります。GitHub の一時的なエラーは、待ったあと、またはプルリクエストの先端が変わったときにやり直し、5 回でやり直しをやめます。

ghost が開いたプルリクエスト（issue から付けたブランチ、またはエージェントが `gh pr create` で作ったもの）が閉じられていると、その issue は記録に残してやり直しません。他の人が `Closes #N` と書いて閉じたプルリクエストだけでは、その issue を飛ばしません。

GitHub を確認する間隔、作業用コピー、マージの手順は [docs/internals.md](docs/internals.md) にあります。
