use std::path::Path;

use crate::lang::Lang;
use crate::watch::Note;

fn rules(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "\
GitHub のルール（必須）:
- GitHub の操作はすべて `gh` CLI を使う。PR / issue の閲覧、checks、コメント、レビュー、PR 作成を含む。
- GitHub の REST / GraphQL API を直接呼ばない。
- curl、wget、その他の HTTP クライアントで api.github.com にアクセスしない。
- Octokit、octocrab、その他の GitHub API ライブラリを使わない。
- git はローカルの diff、commit、pull request ブランチへの push に使ってよい。
- git はパスフレーズや確認を聞いてこない。`git push` や `git fetch` が失敗したらエラーを報告して止まる。待ったりループで再試行したりしない。
- マージしない。`gh pr merge` を実行しない。
- CI を poll、sleep、wait しない。このターンの作業が終わったら終了する。ghost が `gh` を poll し、変化があったときだけこの同じセッションに follow-up を送る。
思考の過程もユーザーへの返答も日本語で行う。
コードとコミットメッセージは英語のままでよい。
PR の説明とレビューコメントは、リポジトリが明らかに英語で書かれている場合を除き日本語にする。
コミットメッセージ、PR タイトル、PR 本文に ghost と書かない。
",
        Lang::En => "\
GitHub rules (mandatory):
- Use the `gh` CLI for every GitHub action: viewing pull requests and issues, checks, comments, reviews, and opening pull requests.
- Do not call the GitHub REST or GraphQL API directly.
- Do not use curl, wget, or any other HTTP client against api.github.com.
- Do not use Octokit, octocrab, or any GitHub API library.
- git is allowed for local diffs, commits, and pushing the pull request branch.
- git will not prompt for a passphrase or confirmation. If `git push` or `git fetch` fails, report the error and stop. Do not wait or retry in a loop.
- Never merge. Never run `gh pr merge`.
- Do not poll, sleep, or wait on CI. Finish this turn when the task is done. ghost polls `gh` itself and will send a follow-up in this same session only when something changes.
Think aloud and reply to the user in English.
Code and commit messages may stay in English.
Write pull request descriptions and review comments in English.
Do not write the word ghost into commit messages, pull request titles, or pull request bodies.
",
    }
}

fn header(lang: Lang, role: &str, slug: &str, workdir: &Path) -> String {
    let intro = lang.say(
        "最初に、これから何をするか一行で述べる。",
        "Begin with one short line saying what you are about to do.",
    );
    format!(
        "{role}\nRepository: {slug}\nWorking directory: {wd}\n\n{rules}\n{intro}\n",
        wd = workdir.display(),
        rules = rules(lang),
    )
}

fn role_review(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "このリポジトリのレビュー担当です。",
        Lang::En => "You are the review agent for this repository.",
    }
}

fn role_code(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "このリポジトリのコーディング担当です。",
        Lang::En => "You are the coding agent for this repository.",
    }
}

pub fn review_initial(
    slug: &str,
    number: u64,
    head_sha: &str,
    workdir: &Path,
    diff: &str,
    user_notes: &[String],
    lang: Lang,
) -> String {
    let body = match lang {
        Lang::Ja => format!(
            "プルリクエスト #{number}（{slug}）をレビューする。\n\
             Head SHA: {head_sha}\n\
             これはこのセッションでの最初のレビューで、PR 全体の diff です。\n\
             `gh` でレビューを投稿する:\n\
             - 問題なければ approve: `gh pr review {number} --repo {slug} --approve --body \"...\"`\n\
             - それ以外は comment または request changes: `gh pr review {number} --repo {slug} --comment --body \"...\"` または `--request-changes`\n"
        ),
        Lang::En => format!(
            "You are reviewing pull request #{number} ({slug}).\n\
             Head SHA: {head_sha}\n\
             This is the whole pull request diff (first review in this session).\n\
             Post a review with `gh`:\n\
             - approve when the change is fine: `gh pr review {number} --repo {slug} --approve --body \"...\"`\n\
             - otherwise comment or request changes: `gh pr review {number} --repo {slug} --comment --body \"...\"` or `--request-changes`\n"
        ),
    };
    format!(
        "{head}\n{body}{notes}\nDiff:\n```diff\n{diff}\n```\n",
        head = header(lang, role_review(lang), slug, workdir),
        notes = notes_block(user_notes, lang),
        diff = fence_safe(diff),
    )
}

pub fn review_followup(
    slug: &str,
    number: u64,
    previous_sha: &str,
    head_sha: &str,
    workdir: &Path,
    diff: &str,
    user_notes: &[String],
    lang: Lang,
) -> String {
    let body = match lang {
        Lang::Ja => format!(
            "これは同じレビューセッションの follow-up です。最初からやり直さず、レビュー済みのコミットを繰り返さない。\n\
             プルリクエスト #{number}（{slug}）は {previous_sha} から {head_sha} に進みました。\n\
             新しい diff だけをレビューし、`gh pr review {number} --repo {slug}` で新しいレビューを投稿する。\n\
             問題なければ `--approve`、そうでなければ `--comment` または `--request-changes`。\n"
        ),
        Lang::En => format!(
            "Follow-up in this same review session. Do not start over and do not rehash commits you already reviewed.\n\
             Pull request #{number} ({slug}) moved from {previous_sha} to {head_sha}.\n\
             Review only the new diff and post a new review with `gh pr review {number} --repo {slug}`.\n\
             Approve with `--approve` when the pull request is in good shape, otherwise `--comment` or `--request-changes`.\n"
        ),
    };
    format!(
        "{head}\n{body}{notes}\nDiff since the last reviewed SHA:\n```diff\n{diff}\n```\n",
        head = header(lang, role_review(lang), slug, workdir),
        notes = notes_block(user_notes, lang),
        diff = fence_safe(diff),
    )
}

pub fn code_initial(
    slug: &str,
    number: u64,
    branch: &str,
    head_sha: &str,
    workdir: &Path,
    ci_failed: bool,
    failing: &[String],
    notes: &[Note],
    user_notes: &[String],
    lang: Lang,
) -> String {
    let intro = match lang {
        Lang::Ja => format!(
            "プルリクエスト #{number}（{slug}）の作者として直す。ブランチ `{branch}`、head {head_sha}。\n\
             作業ディレクトリにそのブランチがチェックアウトされている。そこで commit し、同じブランチへ `git push` する。\n"
        ),
        Lang::En => format!(
            "You are the author of pull request #{number} ({slug}), branch `{branch}`, head {head_sha}.\n\
             The branch is checked out in the working directory. Commit there and `git push` the same branch.\n"
        ),
    };
    let fix = match lang {
        Lang::Ja => format!(
            "コードを直し、push し、対応したコメントには `gh pr comment {number} --repo {slug} --body \"...\"` で返信する。\n"
        ),
        Lang::En => format!(
            "Fix the code, push, and reply to each comment you addressed with `gh pr comment {number} --repo {slug} --body \"...\"`.\n"
        ),
    };
    format!(
        "{head}\n{intro}{situation}\n{fix}{user}",
        head = header(lang, role_code(lang), slug, workdir),
        situation = code_situation(ci_failed, failing, notes, slug, number, lang),
        user = notes_block(user_notes, lang),
    )
}

pub fn code_followup(
    slug: &str,
    number: u64,
    branch: &str,
    head_sha: &str,
    workdir: &Path,
    ci_failed: bool,
    failing: &[String],
    notes: &[Note],
    user_notes: &[String],
    lang: Lang,
) -> String {
    let intro = match lang {
        Lang::Ja => format!(
            "プルリクエスト #{number}（{slug}）、ブランチ `{branch}`、head {head_sha} の、同じコーディングセッションでの follow-up です。\n\
             すでにある文脈を保つ。作業を最初からやり直さない。マージしない。\n"
        ),
        Lang::En => format!(
            "Follow-up in this same coding session for pull request #{number} ({slug}), branch `{branch}`, head {head_sha}.\n\
             Keep the context you already have. Do not restart the work and do not merge.\n"
        ),
    };
    let fix = match lang {
        Lang::Ja => {
            format!("修正を `{branch}` に push し、対応したコメントには `gh` で返信する。\n")
        }
        Lang::En => {
            format!("Push fixes to `{branch}` and reply with `gh` to comments you addressed.\n")
        }
    };
    format!(
        "{head}\n{intro}{situation}\n{fix}{user}",
        head = header(lang, role_code(lang), slug, workdir),
        situation = code_situation(ci_failed, failing, notes, slug, number, lang),
        user = notes_block(user_notes, lang),
    )
}

pub fn issue_initial(
    slug: &str,
    number: u64,
    title: &str,
    body: &str,
    branch: &str,
    base: &str,
    workdir: &Path,
    user_notes: &[String],
    lang: Lang,
) -> String {
    let body = truncate(body, 12_000);
    let task = match lang {
        Lang::Ja => format!(
            "{slug} の GitHub issue #{number} を実装する。\n\
             Title: {title}\n\n\
             Issue body:\n{body}\n\n\
             ブランチ名の規則: `ghost/` プレフィックスは使わない。`<type>/<issue番号>-<短いケバブケース>` とする。\
             type は feat, fix, docs, refactor, chore, test のいずれかで、issue のタイトルと本文から選ぶ。\
             slug はタイトル由来の ASCII 小文字ケバブケースで、最大約 40 文字。トークンの途中で切らない。\
             ASCII の slug が無ければ `<type>/<番号>` とする（例: feat/12）。例: fix/12-crash-on-empty-config。\n\
             チェックアウト済みのブランチは `{branch}`（base `{base}`）です。この名前で commit し、`git push -u origin {branch}` する。\n\
             まだ PR が無ければ `gh` で開く:\n\
             `gh pr create --repo {slug} --base {base} --head {branch} --title \"...\" --body-file <file>`\n\
             PR 本文には、マージで issue が自動クローズされるよう、必ず次の行を入れる: `Closes #{number}`\n\
             その PR が既にあるなら重複して開かない。マージしない。\n\
             詳細が要るときは `gh issue view {number} --repo {slug}` を読む。\n"
        ),
        Lang::En => format!(
            "Implement GitHub issue #{number} in {slug}.\n\
             Title: {title}\n\n\
             Issue body:\n{body}\n\n\
             Branch names must not use a ghost/ prefix. Use `<type>/<issue-number>-<short-kebab-slug>`.\
             type is one of feat, fix, docs, refactor, chore, test, chosen from the issue title and body.\
             The slug is ASCII lowercase kebab-case from the title, about 40 characters, and must not end mid-token.\
             If there is no ASCII slug, use `<type>/<number>` (for example feat/12). Example: fix/12-crash-on-empty-config.\n\
             Branch `{branch}` is checked out from `{base}`. Commit there and `git push -u origin {branch}`.\n\
             Open a pull request with `gh` if one does not already exist:\n\
             `gh pr create --repo {slug} --base {base} --head {branch} --title \"...\" --body-file <file>`\n\
             The pull request body must include a GitHub closing keyword line `Closes #{number}` so merging auto-closes the issue.\n\
             Do not open a duplicate if that pull request already exists. Do not merge.\n\
             Read the issue with `gh issue view {number} --repo {slug}` if you need more detail.\n"
        ),
    };
    format!(
        "{head}\n{task}{user}",
        head = header(lang, role_code(lang), slug, workdir),
        user = notes_block(user_notes, lang),
    )
}

pub fn user_nudge(lang: Lang, text: &str, during_turn: bool) -> String {
    let lead = match (lang, during_turn) {
        (Lang::Ja, false) => {
            "ユーザーが ghost にこれを入力した。この同じセッションのまま続ける。GitHub の操作は `gh` CLI だけを使う。マージしない。"
        }
        (Lang::Ja, true) => {
            "前のターンの実行中にユーザーが ghost へこれを入力した。この同じセッションのまま続ける。GitHub の操作は `gh` CLI だけを使う。マージしない。"
        }
        (Lang::En, false) => {
            "The user typed this into the session. Stay in this same session. Use the `gh` CLI for any GitHub action. Do not merge."
        }
        (Lang::En, true) => {
            "The user typed this while the previous turn was running. Stay in this same session. Use the `gh` CLI for any GitHub action. Do not merge."
        }
    };
    format!("{lead}\n\n{text}")
}

pub fn issue_resume_prefix(lang: Lang, number: u64, branch: &str) -> String {
    match lang {
        Lang::Ja => format!(
            "同じセッションの follow-up です。issue #{number} の実装をブランチ `{branch}` で続ける。`Closes #{number}` を本文に含むプルリクエストがあることを確認する。重複して開かない。マージしない。\n\n"
        ),
        Lang::En => format!(
            "Follow-up in this same session. Continue implementing issue #{number} on `{branch}` and make sure a pull request whose body contains `Closes #{number}` exists. Do not open a duplicate. Do not merge.\n\n"
        ),
    }
}

fn code_situation(
    ci_failed: bool,
    failing: &[String],
    notes: &[Note],
    slug: &str,
    number: u64,
    lang: Lang,
) -> String {
    let mut parts = Vec::new();
    if ci_failed {
        let names = if failing.is_empty() {
            "(see `gh pr checks`)".to_string()
        } else {
            failing.join(", ")
        };
        parts.push(match lang {
            Lang::Ja => format!(
                "CI が失敗している: {names}。`gh pr checks {number} --repo {slug}` と `gh run view` で確認する。"
            ),
            Lang::En => format!(
                "CI failed: {names}. Inspect with `gh pr checks {number} --repo {slug}` and `gh run view`."
            ),
        });
    }
    if notes.is_empty() {
        parts.push(lang.say("新しいレビューコメントはない。", "No new review comments."));
    } else {
        parts.push(lang.say(
            "対応する新しいレビューコメント:",
            "New review comments to address:",
        ));
        for note in notes.iter().take(15) {
            parts.push(format!(
                "- {author} ({kind}, id {id}): {body}",
                author = note.author,
                kind = note.kind,
                id = note.id,
                body = truncate(&note.body, 2_000),
            ));
        }
    }
    parts.join("\n")
}

fn notes_block(notes: &[String], lang: Lang) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = lang.say(
        "\nユーザーが入力したメモ:\n",
        "\nNotes the user typed in:\n",
    );
    for note in notes {
        out.push_str("- ");
        out.push_str(&truncate(note, 2_000));
        out.push('\n');
    }
    out
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n…[truncated; inspect the rest locally with gh or git]");
    out
}

fn fence_safe(diff: &str) -> String {
    truncate(&diff.replace("```", "'''"), 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn prompts_require_gh_and_forbid_merge() {
        let dir = Path::new("/home/u/.ghost/work/acme__app");
        let review = review_initial("acme/app", 3, "abc", dir, "diff", &[], Lang::En);
        assert!(review.contains("gh"));
        assert!(review.contains("Never merge"));
        assert!(review.contains("will not prompt"));
        assert!(review.contains("api.github.com"));
        assert!(review.contains("--approve"));

        let follow = review_followup("acme/app", 3, "aaa", "bbb", dir, "d", &[], Lang::En);
        assert!(follow.contains("same review session"));
        assert!(follow.contains("aaa"));
        assert!(follow.contains("bbb"));

        let code = code_followup(
            "acme/app",
            3,
            "feature",
            "abc",
            dir,
            true,
            &["build".into()],
            &[],
            &[],
            Lang::En,
        );
        assert!(code.contains("same coding session"));
        assert!(code.contains("Do not restart"));
        assert!(code.contains("gh pr checks"));
        assert!(code.contains("Never merge"));

        let issue = issue_initial(
            "acme/app",
            7,
            "Crash",
            "boom",
            "fix/7-crash",
            "main",
            dir,
            &[],
            Lang::En,
        );
        assert!(issue.contains("Closes #7"));
        assert!(issue.contains("gh pr create"));
        assert!(issue.contains("gh issue view"));
        assert!(issue.contains("fix/12-crash-on-empty-config"));
        assert!(issue.contains("feat/12"));
        assert!(!issue.contains("ghost/issue"));
    }

    #[test]
    fn japanese_prompts_ask_for_japanese_and_a_closing_line() {
        let dir = Path::new("/home/u/.ghost/work/acme__app");
        let issue = issue_initial(
            "acme/app",
            7,
            "落ちる",
            "本文",
            "fix/7",
            "main",
            dir,
            &[],
            Lang::Ja,
        );
        assert!(issue.contains("日本語"));
        assert!(issue.contains("Closes #7"));
        assert!(issue.contains("fix/12-crash-on-empty-config"));
        assert!(issue.contains("マージしない"));
        assert!(!issue.contains("ghost/issue"));
        assert!(issue.contains("コーディング担当"));
        let review = review_initial("acme/app", 3, "abc", dir, "diff", &[], Lang::Ja);
        assert!(review.contains("日本語"));
        assert!(review.contains("マージしない"));
    }
}
