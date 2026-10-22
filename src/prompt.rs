use std::path::Path;

use crate::watch::Note;

const RULES: &str = "\
GitHub rules (mandatory):
- Use the `gh` CLI for every GitHub action: viewing pull requests and issues, checks, comments, reviews, and opening pull requests.
- Do not call the GitHub REST or GraphQL API directly.
- Do not use curl, wget, or any other HTTP client against api.github.com.
- Do not use Octokit, octocrab, or any GitHub API library.
- git is allowed for local diffs, commits, and pushing the pull request branch.
- Never merge. Never run `gh pr merge`.
- Do not poll, sleep, or wait on CI. Finish this turn when the task is done. ghost polls `gh` itself and will send a follow-up in this same session only when something changes.
";

fn header(role: &str, slug: &str, workdir: &Path) -> String {
    format!(
        "{role}\nRepository: {slug}\nWorking directory: {wd}\n\n{RULES}\nBegin with one short line saying what you are about to do.\n",
        wd = workdir.display()
    )
}

pub fn review_initial(
    slug: &str,
    number: u64,
    head_sha: &str,
    workdir: &Path,
    diff: &str,
    user_notes: &[String],
) -> String {
    format!(
        "{head}\n\
         You are the reviewer for pull request #{number} ({slug}).\n\
         Head SHA: {head_sha}\n\
         This is the whole pull request diff (first review in this session).\n\
         Post a review with `gh`:\n\
         - approve when the change is fine: `gh pr review {number} --repo {slug} --approve --body \"...\"`\n\
         - otherwise comment or request changes: `gh pr review {number} --repo {slug} --comment --body \"...\"` or `--request-changes`\n\
         {notes}\n\
         Diff:\n```diff\n{diff}\n```\n",
        head = header("You are ghost's review agent.", slug, workdir),
        notes = notes_block(user_notes),
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
) -> String {
    format!(
        "{head}\n\
         Follow-up in this same review session. Do not start over and do not rehash commits you already reviewed.\n\
         Pull request #{number} ({slug}) moved from {previous_sha} to {head_sha}.\n\
         Review only the new diff and post a new review with `gh pr review {number} --repo {slug}`.\n\
         Approve with `--approve` when the pull request is in good shape, otherwise `--comment` or `--request-changes`.\n\
         {notes}\n\
         Diff since the last reviewed SHA:\n```diff\n{diff}\n```\n",
        head = header("You are ghost's review agent.", slug, workdir),
        notes = notes_block(user_notes),
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
) -> String {
    format!(
        "{head}\n\
         You are the author of pull request #{number} ({slug}), branch `{branch}`, head {head_sha}.\n\
         The branch is checked out in the working directory. Commit there and `git push` the same branch.\n\
         {situation}\n\
         Fix the code, push, and reply to each comment you addressed with `gh pr comment {number} --repo {slug} --body \"...\"`.\n\
         {user}\n",
        head = header("You are ghost's coding agent.", slug, workdir),
        situation = code_situation(ci_failed, failing, notes, slug, number),
        user = notes_block(user_notes),
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
) -> String {
    format!(
        "{head}\n\
         Follow-up in this same coding session for pull request #{number} ({slug}), branch `{branch}`, head {head_sha}.\n\
         Keep the context you already have. Do not restart the work and do not merge.\n\
         {situation}\n\
         Push fixes to `{branch}` and reply with `gh` to comments you addressed.\n\
         {user}\n",
        head = header("You are ghost's coding agent.", slug, workdir),
        situation = code_situation(ci_failed, failing, notes, slug, number),
        user = notes_block(user_notes),
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
) -> String {
    format!(
        "{head}\n\
         Implement GitHub issue #{number} in {slug}.\n\
         Title: {title}\n\n\
         Issue body:\n{body}\n\n\
         The branch `{branch}` is checked out from `{base}`. Implement the issue, commit, and `git push -u origin {branch}`.\n\
         Open a pull request with `gh` if one does not already exist:\n\
         `gh pr create --repo {slug} --base {base} --head {branch} --title \"...\" --body $'...\\n\\nCloses #{number}'`\n\
         The body must contain `Closes #{number}`. Do not open a duplicate if that pull request already exists. Do not merge.\n\
         Read the issue with `gh issue view {number} --repo {slug}` if you need more detail.\n\
         {user}\n",
        head = header("You are ghost's coding agent.", slug, workdir),
        body = truncate(body, 12_000),
        user = notes_block(user_notes),
    )
}

fn code_situation(
    ci_failed: bool,
    failing: &[String],
    notes: &[Note],
    slug: &str,
    number: u64,
) -> String {
    let mut parts = Vec::new();
    if ci_failed {
        let names = if failing.is_empty() {
            "(see `gh pr checks`)".to_string()
        } else {
            failing.join(", ")
        };
        parts.push(format!(
            "CI failed: {names}. Inspect with `gh pr checks {number} --repo {slug}` and `gh run view`."
        ));
    }
    if notes.is_empty() {
        parts.push("No new review comments.".into());
    } else {
        parts.push("New review comments to address:".into());
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

fn notes_block(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nNotes the user typed into ghost:\n");
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
        let review = review_initial("acme/app", 3, "abc", dir, "diff", &[]);
        assert!(review.contains("gh"));
        assert!(review.contains("Never merge"));
        assert!(review.contains("api.github.com"));
        assert!(review.contains("--approve"));

        let follow = review_followup("acme/app", 3, "aaa", "bbb", dir, "d", &[]);
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
            "ghost/issue-7",
            "main",
            dir,
            &[],
        );
        assert!(issue.contains("Closes #7"));
        assert!(issue.contains("gh pr create"));
        assert!(issue.contains("gh issue view"));
    }
}
