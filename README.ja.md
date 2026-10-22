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

`--interval` のデフォルトは review、code、issue → code のすべてで `20s` です（`30s`、`2m`、`1h30m`、または単位なしの秒数）。`--agent codex|claude|grok` は、デフォルト（`PATH` 上で codex → claude → grok の順で最初に見つかったもの）を上書きします。`--lang ja|en` のデフォルトは `ja`（`GHOST_LANG`）です。`--ssh-key PATH`（`GHOST_SSH_KEY`）は ghost の git と内側のエージェントに `ssh -i PATH -o IdentitiesOnly=yes` を設定します。鍵を指定しない場合、ghost は identity を強制しません（`~/.ssh/id_rsa` をデフォルトにしません）。`-y` / `--yes` は続行プロンプトを受け入れます。`--max-iterations`（デフォルト 20）はこのプロセスのエージェントターンの上限です。`--trust-comments collaborators|contributors|all`（デフォルト `collaborators`）は、PR 作者とリポジトリオーナー以外に、だれのコメントが code モードを起こすかを選びます。

フラグなしの PR URL は review か code かを尋ねます。リポジトリ URL に `--review` / `--code` を付けるのはエラーです。issue URL に `--review` を付けるのもエラーです。オープンな issue も PR も無いリポジトリは一行だけ出して終了コード 0 で終わり、ピッカーには入りません。

![ピッカー](docs/screenshots/picker.png)

ghost は `gh` だけで poll します。各 poll は `gh api` で取ったレビュー本文、インラインコメント、issue コメントの件数を `reviews N, inline N, comments N` とログします。最初の実作業でエージェントを起動し、その後の更新は同じセッションへの follow-up です。更新が無い poll（review では SHA が同じ、code では信頼できる新しいフィードバックも新しい CI 失敗シグネチャも無い）はエージェントを呼びません。失敗したターンでは SHA やコメントを処理済みにしません。resume の失敗は、同じ文脈の新しいセッションで一度だけ再試行します。同じ SHA の同じ CI 失敗は、SHA か失敗チェックの集合が変わるまで再送しません。

`issue/N` が PR を開く（または既存の PR を見つける）と、デフォルトでは日本語で `PR #M を --code として続けますか？ [Y/n]` と尋ね、issue の文脈を持つ同じセッションを維持します。PR 作成より後に付いたコメントは、その引き継ぎ時に既読にしません。エージェント自身の返信は後から無視しますが、同じ GitHub ユーザーによるレビューは無視しません。ghost が PR 本文を編集するとき、`Closes #N` の行が無ければ足します。issue のブランチは `fix/12-crash-on-empty-config` の形で、`ghost/issue-N` にはしません。

PR がマージ可能、マージ済み、またはクローズになるとエージェントは止まります。ghost はマージしません。エージェントの `PATH` の先頭には、`gh pr merge` や `gh repo delete` などの破壊的なコマンドを拒否する `gh` ラッパーがあります。

![同じセッションの code モード](docs/screenshots/code.png)

継続フラグ:

- Claude: `claude --print --dangerously-skip-permissions --input-format stream-json --output-format stream-json --verbose --session-id <uuid>`。続きのプロンプトは stdin の JSON。プロセスが終了したら同じフラグで `--resume <uuid>`（`--session-id` ではない）。
- Codex: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <dir> -`。その後は同じフラグの `codex exec resume <thread_id>`。
- Grok: `grok --prompt-file <file> --session-id <uuid> --output-format streaming-json --yolo --cwd <dir>`。続きは `--session-id` なしの `--resume <uuid>`。

プロンプトは、GitHub の操作に `gh` を使うこと、選んだ言語で返答すること、マージしないことをエージェントに伝えます。

![--code として続ける](docs/screenshots/confirm.png)

## 作業ディレクトリ

クローンは `~/.ghost/work/OWNER__REPO` です（`GHOST_HOME` が `~/.ghost` を上書きします）。ghost は `gh repo clone` でクローンし、終了後も残します。PR または issue ごとに `~/.ghost/work/OWNER__REPO.wt/` 配下の git worktree を使うので、同じリポジトリに対する二つの ghost プロセスがチェックアウトを共有しません。ロックファイルが、同じスロットの二重使用を止めます。クローン、fetch、最初のチェックアウトは TUI の前に、通常の端末で行います。`origin` が一致する既存のクローンは fetch し、汚れたツリーは `autostash` で stash します。壊れたチェックアウトは脇へ退けてクローンし直します。エージェントの cwd はその worktree であり、シェルの cwd ではありません。

## トラブルシューティング

`Enter passphrase for key` のプロンプトは TUI が始まる前に答えます。その後 ghost 自身が走らせる `git` / `gh` と、エージェントの `git push` はプロンプトしません。ssh は `BatchMode`、`GIT_TERMINAL_PROMPT=0` です。鍵がロックされていて fetch や push が失敗すると、TUI にエラーが出ます。`ssh-add` で鍵を unlock するか、`--ssh-key` を渡すか、`gh config set git_protocol https` でリモートを HTTPS にしてから ghost を再実行します。コマンドはハングせずタイムアウトし、GitHub の rate limit はバックオフして再試行します。

最後にレビューした SHA、既読コメント id、処理済みの CI シグネチャ、セッション id は `$GHOST_STATE_DIR`、または `$XDG_STATE_HOME/ghost`、または `~/.local/state/ghost` に保存します。パニックまたは SIGTERM では端末を戻し、エージェントのプロセスグループにシグナルを送ります。

マージ可能とは、オープン（draft ではない）、チェックが成功または不在、approve が一つ以上、未解決の changes requested が無く、mergeable であることです。同じレビュアーの後からの `COMMENTED` は、それより前の approve を消しません。

`q` で終了します。`i` で同じセッションにメッセージを送ります。`ctrl-c` はターンを中断し、セッション id は残します。`ghost --demo picker|review|code|confirm` は上の画面を台本データで描きます。
