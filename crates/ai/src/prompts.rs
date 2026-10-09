//! Prompts for the context actions (explain a diff, write a commit message, resolve a conflict,
//! summarise a PR) and helpers to fit diffs into them.

use git_core::parse::diff::{FileDiff, LineKind};

/// Longest diff text put in a prompt.
pub const MAX_DIFF: usize = 48 * 1024;

pub const COMMIT_SYSTEM: &str = "You write git commit messages. Reply with the message only: \
a summary line in the imperative mood under 72 characters, then (only if useful) a blank line \
and a short body wrapped at 72 characters explaining what changed and why. No code fences, no \
preamble, no trailers.";

/// Unified-diff text for `files`, cut to `max` bytes.
pub fn diff_text(files: &[FileDiff], max: usize) -> String {
    let mut out = String::new();
    for file in files {
        out.push_str(&String::from_utf8_lossy(&file.header));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if file.binary {
            out.push_str("(binary file)\n");
        }
        for hunk in &file.hunks {
            out.push_str(&String::from_utf8_lossy(&hunk.header));
            out.push('\n');
            for line in &hunk.lines {
                let sign = match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Deleted => '-',
                    LineKind::NoNewlineAtEof => '\\',
                };
                out.push(sign);
                if line.kind == LineKind::NoNewlineAtEof {
                    out.push_str(" No newline at end of file");
                } else {
                    out.push_str(&String::from_utf8_lossy(&line.content));
                }
                out.push('\n');
            }
        }
        if out.len() > max {
            break;
        }
    }
    crate::event::truncate(&out, max)
}

/// The user message for a commit message from a staged diff.
pub fn commit_message(branch: Option<&str>, diff: &str) -> String {
    let branch = branch
        .map(|b| format!(" on branch `{b}`"))
        .unwrap_or_default();
    format!("Write a commit message for these staged changes{branch}.\n\n```diff\n{diff}\n```")
}

/// The commit-message request as one agent prompt (agents have no separate system prompt).
pub fn commit_message_for_agent(branch: Option<&str>, diff: &str) -> String {
    format!(
        "{COMMIT_SYSTEM} Do not run tools or edit files; everything you need is below.\n\n{}",
        commit_message(branch, diff)
    )
}

/// Splits a generated message into (summary, body), dropping code fences and quotes.
pub fn split_message(text: &str) -> (String, String) {
    let cleaned: Vec<&str> = text
        .trim()
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let text = cleaned.join("\n");
    let text = text.trim();
    let (summary, body) = text.split_once('\n').unwrap_or((text, ""));
    let summary = summary
        .trim()
        .trim_matches(|c| c == '"' || c == '`')
        .trim_start_matches("Summary:")
        .trim()
        .to_string();
    (summary, body.trim().to_string())
}

pub fn explain_diff(what: &str, diff: &str) -> String {
    format!(
        "Explain this diff ({what}): what changed, why it likely changed, and anything risky. \
         Be concise.\n\n```diff\n{diff}\n```"
    )
}

/// Asks for a resolution of one conflicted file. The agent proposes; the user saves.
pub fn resolve_conflict(path: &str, operation: &str, content: &str) -> String {
    format!(
        "The file `{path}` has merge conflicts ({operation}). Propose a resolution: explain \
         each conflict briefly, then give the complete resolved file in one code block. Do not \
         edit files or run commands; I will review and save it myself.\n\n```\n{content}\n```"
    )
}

/// Hook for the PR panel: a summary request for a pull request.
pub fn summarise_pr(title: &str, description: &str, diff: &str) -> String {
    format!(
        "Summarise this pull request for a reviewer: purpose, main changes, risks and what to \
         test. Be concise.\n\nTitle: {title}\n\nDescription:\n{description}\n\n```diff\n{diff}\n```"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_text_round_trips_a_patch() {
        let patch = b"diff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n\\ No newline at end of file\n";
        let files = git_core::parse::diff::parse(patch).unwrap();
        let text = diff_text(&files, MAX_DIFF);
        assert!(text.starts_with("diff --git a/f.txt b/f.txt\n"), "{text}");
        assert!(
            text.contains("@@ -1,2 +1,2 @@\n a\n-b\n+c\n\\ No newline"),
            "{text}"
        );
        let cut = diff_text(&files, 20);
        assert!(cut.contains("more bytes"));
    }

    #[test]
    fn splits_messages() {
        assert_eq!(
            split_message("```\nFix parser\n\nHandle CRLF.\n```"),
            ("Fix parser".into(), "Handle CRLF.".into())
        );
        assert_eq!(
            split_message("\"Add tests\""),
            ("Add tests".into(), String::new())
        );
        assert_eq!(
            split_message("Summary: Bump deps\nbody"),
            ("Bump deps".into(), "body".into())
        );
    }

    #[test]
    fn prompts_carry_their_inputs() {
        assert!(commit_message(Some("main"), "D").contains("`main`"));
        assert!(commit_message_for_agent(None, "D").contains("Do not run tools"));
        assert!(resolve_conflict("a.rs", "merge", "<<<<<<<").contains("`a.rs`"));
        assert!(summarise_pr("T", "B", "D").contains("Title: T"));
        assert!(explain_diff("commit abc", "D").contains("commit abc"));
    }
}
