# ghost

[English](README.md) | [日本語](README.ja.md)

GitHub のプルリクエストを、ローカルのコーディングエージェント（Codex、Claude、または Grok）ひとつでマージ可能になるまで見守る TUI です。

![プルリクエストのレビュー](docs/screenshots/review.png)

## インストール

[GitHub CLI](https://cli.github.com/)（`gh auth login`）と、`PATH` 上の `codex`、`claude`、`grok` のいずれかが必要です。

```sh
cargo install --path .
```

起動時に ghost は `gh auth status` と `gh repo view OWNER/REPO` を実行します。失敗すると、TUI が直し方（`gh auth login`、`gh` のインストール、リポジトリ名の確認）を出して終了します。

## 使い方

```sh
ghost github.com/OWNER/REPO                  # オープンな PR と issue のピッカー
ghost github.com/OWNER/REPO/pr/N --review    # head SHA が変わったらレビュー
ghost github.com/OWNER/REPO/pr/N --code      # 赤い CI と新しいレビューコメントを直す
ghost github.com/OWNER/REPO/issue/N          # 実装して PR を開き、--code として続ける
ghost github.com/OWNER/REPO/issue/N --yes    # 続行の確認を自動で受け入れる
```

| 短縮 | 長い形 | デフォルト | |
| --- | --- | --- | --- |
| `-r` | `--review` | | head SHA が変わったらレビュー。`-c` と同時には付けられない。 |
| `-c` | `--code` | | 赤い CI と新しいレビューコメントを直す。`-r` と同時には付けられない。 |
| `-i` | `--interval` | `20s` | poll 間隔（`30s`、`2m`、`1h30m`、または単位なしの秒数）。 |
| `-a` | `--agent` | `PATH` 上の最初 | `codex`、`claude`、`grok`。 |
| `-k` | `--ssh-key` | ssh の設定、または `GHOST_SSH_KEY` | `ssh -i PATH -o IdentitiesOnly=yes`。鍵を省略すると identity を強制しない（`~/.ssh/id_rsa` は仮定しない）。 |
| `-l` | `--lang` | `ja`（フラグが無いとき `GHOST_LANG`） | `ja` または `en`。 |
| `-y` | `--yes` | | issue から `--code` への続行と、レビュー終了の確認を受け入れる。マージはしない。 |
| | `--merge-yes` | | `--code` または issue モードで、マージ可能なら再確認のあと merge する。短縮形はない。 |
| | `--merge-method` | リポジトリの既定 | `merge`、`squash`、`rebase`。`gh pr merge` に渡す。 |
| `-n` | `--max-iterations` | `20` | このプロセスのエージェントターンの上限。 |
| `-t` | `--trust-comments` | `all` | PR 作者とリポジトリオーナー以外に、だれが code モードを起こすか。デフォルトは全員（人間、Copilot、その他の bot、外部の人）。`collaborators` または `contributors` で絞れる。 |
| `-d` | `--demo` | | `picker`、`review`、`code`、`confirm`、`preflight`。 |
| `-h` | `--help` | | ヘルプ。 |
| `-V` | `--version` | | バージョン。 |

フラグなしの PR URL は review か code かを尋ねます。リポジトリ URL に `--review` / `--code` を付けるのはエラーです。issue URL に `--review` を付けるのもエラーです。オープンな issue も PR も無いリポジトリは一行だけ出して終了コード 0 で終わり、ピッカーには入りません。

![ピッカー](docs/screenshots/picker.png)

ghost は `gh` だけで poll します。各 poll は `gh api` で取ったレビュー本文、インラインコメント、issue コメントの件数を `reviews N, inline N, comments N` とログします。code の各 poll は `filtered 0` も出します。`-t` で落とすと `filtered N (untrusted author)` です。デフォルトの `all` では Copilot のレビュー本文（`copilot-pull-request-reviewer[bot]`）とインラインコメント（`Copilot`、または同じ bot ログイン）を残します。起動時は `--review`、`--code`、issue モードのどれも、保存された状態が「新しいことはない」でも、状況確認のターンでエージェントを一度起動します。その後の poll は、変化が無いとき（review では SHA が同じ、code では新しいフィードバックも新しい CI 失敗シグネチャも無い）はエージェントを呼びません。失敗したターンでは SHA やコメントを処理済みにしません。resume の失敗は、同じ文脈の新しいセッションで一度だけ再試行します。エージェントの失敗が続くと待ち時間を延ばし、5 回連続で止みます。同じ SHA の同じ CI 失敗は、SHA か失敗チェックの集合が変わるまで再送しません。

`i` で送った文は必ずエージェントに届きます。ターンの実行中ならキューに入れ、そのターンの直後に送ります。セッションが無い、または落ちているときは、開始または再開してから送ります。ログは `→ エージェントに送信`、またはその句を含む送信待ちを出します。黙って捨てることはありません。

`issue/N` が PR を開く（または既存の PR を見つける）と、デフォルトでは日本語で `PR #M を --code として続けますか？ [Y/n]` と尋ね、issue の文脈を持つ同じセッションを維持します。PR 作成より後に付いたコメントは、その引き継ぎ時に既読にしません。エージェント自身の返信は後から無視しますが、同じ GitHub ユーザーによるレビューは無視しません。ghost が PR 本文を編集するとき、`Closes #N` の行が無ければ足します。issue のブランチは `fix/12-crash-on-empty-config` の形で、`ghost/issue-N` にはしません。

レビューはマージしません。レビューのターンが `GHOST_VERDICT: approve` で終わるか、その行が無く `LGTM / 承認相当` を含む自己レビューのコメントがあるとき、`PR #N は問題なしと判断しました。終了しますか？ [y/c]` と尋ねます。`y`、Enter、Esc、Ctrl-C は終了です。`c` は監視を続け、新しいコミットで再レビューし、次の approve でまた尋ねます。TTY でないときと `-y` は `y` です。`request_changes` と `comment` は poll を続けます。自分の PR で GitHub が approve を拒否するときは、エージェントが `gh pr review --comment` で `LGTM / 承認相当` を投稿し、最後の行は `GHOST_VERDICT: approve` のままにします。

code モードで、ghost 自身の `gh` 再確認がマージ可能なら、`PR #N を merge しますか？ [y/n/c]` と尋ねます。`y` は head SHA が変わっておらず、まだマージ可能であることを再確認してから、ghost 自身が `gh pr merge` を実行します（エージェントの `gh` ラッパーはこの経路にありません）。`n`、Enter、Esc、Ctrl-C はマージせず終了です。`c` はセッションを残し、poll 間隔のあと、次にマージ可能になったらまた尋ねます。`--merge-yes` は `y` と答え、ログに `--merge-yes` で merge したと出します。`-y` はマージしません。TTY でないときは `--merge-yes` が無い限りマージしません。`--merge-yes` は `--review`、ピッカー、`--code` の無い PR URL ではエラーです。エージェントの `PATH` の先頭の `gh` は、誤って `gh pr merge` や `gh repo delete`、`gh api` の DELETE、`mergePullRequest` のような merge エンドポイントを叩いたときのための事故防止です。セキュリティ境界ではありません。デフォルトブランチへの `git push`、`gh` の alias、絶対パスの `/usr/bin/gh`、別の API の形は通ります。本当に止めたいときは、merge 権限の無いトークンを使ってください。

![同じセッションの code モード](docs/screenshots/code.png)

継続フラグ:

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>`。続きのプロンプトは stdin の JSON。プロセスが終了したら同じフラグで `--resume <uuid>`（`--session-id` ではない）。
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`。続きは `codex --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check resume <thread_id>`（resume に `-C` も `-s` も付けない）。
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`。続きは `--session-id` なしの `--resume <uuid>`。

プロンプトは、GitHub の操作に `gh` を使うこと、選んだ言語で返答すること、マージしないことをエージェントに伝えます。

補助の文字は ANSI の灰色で、明るい黒は使いません。`NO_COLOR` が空でない値のときは色を消し、未設定か空のときは色を付けます。

![--code として続ける](docs/screenshots/confirm.png)

## 作業ディレクトリ

クローンは `~/.ghost/work/OWNER__REPO` です（`GHOST_HOME` が `~/.ghost` を上書きします）。ghost は `gh repo clone` でクローンし、終了後も残します。メインのクローンはデフォルトブランチか detached HEAD のままです。PR のブランチはそこではチェックアウトしません。古い版が残した `ghost/issue-1` のようなブランチも、汚れていれば stash して（`ghost: detach main clone off <branch>`）から外します。ブランチは `~/.ghost/work/OWNER__REPO.wt/` 配下の PR ごとの worktree に移します。`git fetch origin pull/N/head` のあと、その head ブランチを worktree に追加します。ロックファイルが、同じスロットの二重使用を止めます。クローン、fetch、最初のチェックアウトは TUI の前に、通常の端末で行います。`origin` が一致する既存のクローンは fetch し、汚れたツリーは `autostash` で stash します。壊れたチェックアウトは脇へ退けてクローンし直します。

クローン、fetch、worktree のチェックアウトが失敗しても ghost は終了しません。TUI に、その手順の目的、実行したコマンド、終了コード、切り詰めた標準エラー、目指す状態を日本語で出し、エージェントに gh と git で自分の方法でその状態へ到達するよう頼みます。作業ディレクトリは、まだ使えるいちばん近いパスです（スロットの worktree、クローン、または `~/.ghost/work`）。終了するのは、`gh` が無い、`gh` が未ログイン、またはコーディングエージェントが `PATH` に無いときだけです。

## トラブルシューティング

`Enter passphrase for key` のプロンプトは TUI が始まる前に答えます。その後 ghost 自身が走らせる `git` / `gh` と、エージェントの `git push` はプロンプトしません。ssh は `BatchMode`、`GIT_TERMINAL_PROMPT=0` です。鍵がロックされていて fetch や push が失敗すると、TUI にエラーが出ます。`ssh-add` で鍵を unlock するか、`--ssh-key` を渡すか、`gh config set git_protocol https` でリモートを HTTPS にしてから ghost を再実行します。コマンドはハングせずタイムアウトし、GitHub の rate limit はバックオフして再試行します。

最後にレビューした SHA、既読コメント id、処理済みの CI シグネチャ、セッション id は `$GHOST_STATE_DIR`、または `$XDG_STATE_HOME/ghost`、または `~/.local/state/ghost` に保存します。パニックまたは SIGTERM では端末を戻し、エージェントのプロセスグループにシグナルを送ります。

マージ可能とは、オープンで draft ではなく、`mergeStateStatus` が CLEAN、チェックが登録されてすべて成功（チェックが無いのは成功ではない）、コンフリクトがなく、未解決の changes requested が無いことです。同じレビュアーの後からの `COMMENTED` は、それより前の approve を消しません。

`q` で終了します。`i` で同じセッションにメッセージを送ります。`ctrl-c` はターンを中断し、セッション id は残します。`ghost --demo picker|review|code|confirm` は上の画面を台本データで描きます。
