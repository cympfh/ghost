<p align="center"><img src="docs/logo.svg" width="120" alt="ghost"></p>

# ghost

[English](README.md) | [日本語](README.ja.md)

ghost はローカルのコーディングエージェントにプルリクエストをレビューさせたり、issue から実装させたり、CI の失敗とレビューコメントを直させたりします。マージできる状態になると、あなたに確認してから、ghost がマージします。

## 必要なもの

[GitHub CLI](https://cli.github.com/) と、`PATH` 上の `codex`、`claude`、`grok`、`kimi` のいずれかが必要です。ログインには `gh auth login` を使います。

```sh
cargo install --git https://github.com/cympfh/ghost
```

## 使い方

```sh
ghost github.com/OWNER/REPO                  # 開いているプルリクエストか issue を選ぶ
ghost github.com/OWNER/REPO/pull/N --review  # レビューする
ghost github.com/OWNER/REPO/pull/N --code    # CI の失敗とレビューコメントを直す
ghost github.com/OWNER/REPO/pulls --code     # 開いているプルリクエストを選んで直す
ghost github.com/OWNER/REPO/issues/N         # 実装してプルリクエストを開き、直し続ける
ghost github.com/OWNER/REPO/issues --auto    # いちばん古い issue から順に進める
```

エージェントは許可の確認を切った状態で動きます。--auto は issue の本文に従って、確認せずに作業します。信頼できるリポジトリでだけ使ってください。詳しくは [Agent permissions](docs/internals.md#agent-permissions) を見てください。

詳しい説明は [docs/internals.md](docs/internals.md) にあります。
