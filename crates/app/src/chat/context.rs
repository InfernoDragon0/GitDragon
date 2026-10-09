//! Context actions: ask the repository's chat about a diff / conflict / PR, and generate commit
//! messages (ACP adapter one-shot, else the Claude API with the user's key).
//!
//! Each `ask_*` shows a short label in the chat at once, switches to the Chat view, then sends
//! the full prompt once its diff / file has loaded in the background.

use std::future::Future;

use bstr::BString;
use git_core::{CliRepo, DiffTarget, GitRepo as _};
use gpui_kit::{App, Entity, Task, Window};

use crate::repo_tab::RepoTab;

/// Shows `label` in the tab's chat, opens the Chat view, then sends what `prompt` produces.
fn ask_with<F>(tab: &Entity<RepoTab>, label: String, prompt: F, window: &mut Window, cx: &mut App)
where
    F: Future<Output = Result<String, String>> + Send + 'static,
{
    let session = super::session_for(tab, cx);
    session.update(cx, |s, cx| s.announce(label, cx));
    window.dispatch_action(Box::new(crate::actions::ShowChat), cx);
    let job = cx.background_executor().spawn(prompt);
    cx.spawn(async move |cx| {
        let prompt = job.await;
        session.update(cx, |s, cx| match prompt {
            Ok(prompt) => s.enqueue(prompt, cx),
            Err(err) => s.fail(err, cx),
        });
    })
    .detach();
}

fn repo_of(tab: &Entity<RepoTab>, cx: &App) -> Option<CliRepo> {
    tab.read(cx).repo.clone()
}

async fn diff_of(repo: CliRepo, target: DiffTarget) -> Result<String, String> {
    let files = repo.diff(target).await.map_err(|e| e.to_string())?;
    Ok(ai::prompts::diff_text(&files, ai::prompts::MAX_DIFF))
}

/// "Explain this diff" for a commit.
pub fn explain_commit(tab: &Entity<RepoTab>, id: String, window: &mut Window, cx: &mut App) {
    let Some(repo) = repo_of(tab, cx) else { return };
    let short = id[..id.len().min(10)].to_string();
    let label = format!("Explain commit {short}");
    ask_with(
        tab,
        label,
        async move {
            let diff = diff_of(repo, DiffTarget::Commit { id }).await?;
            Ok(ai::prompts::explain_diff(&format!("commit {short}"), &diff))
        },
        window,
        cx,
    );
}

/// "Explain this diff" for the uncommitted changes (staged, then unstaged).
pub fn explain_wip(tab: &Entity<RepoTab>, window: &mut Window, cx: &mut App) {
    let Some(repo) = repo_of(tab, cx) else { return };
    ask_with(
        tab,
        "Explain my uncommitted changes".into(),
        async move {
            let staged = diff_of(repo.clone(), DiffTarget::Staged { paths: Vec::new() }).await?;
            let unstaged = diff_of(repo, DiffTarget::Unstaged { paths: Vec::new() }).await?;
            if staged.trim().is_empty() && unstaged.trim().is_empty() {
                return Err("There are no tracked changes to explain.".into());
            }
            let diff = ai::event::truncate(
                &format!("# Staged\n{staged}\n# Not staged\n{unstaged}"),
                ai::prompts::MAX_DIFF,
            );
            Ok(ai::prompts::explain_diff("uncommitted changes", &diff))
        },
        window,
        cx,
    );
}

/// "Explain this diff" for one working-copy file (`staged` picks the index side).
pub fn explain_file(
    tab: &Entity<RepoTab>,
    path: BString,
    staged: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repo) = repo_of(tab, cx) else { return };
    let shown = String::from_utf8_lossy(&path).into_owned();
    let side = if staged { "staged" } else { "unstaged" };
    ask_with(
        tab,
        format!("Explain the {side} changes in {shown}"),
        async move {
            let paths = vec![path];
            let target = if staged {
                DiffTarget::Staged { paths }
            } else {
                DiffTarget::Unstaged { paths }
            };
            let diff = diff_of(repo, target).await?;
            if diff.trim().is_empty() {
                return Err(format!("No {side} changes in {shown}."));
            }
            Ok(ai::prompts::explain_diff(
                &format!("{side} changes in {shown}"),
                &diff,
            ))
        },
        window,
        cx,
    );
}

/// "Resolve this conflict": asks for a resolution of one conflicted file. The agent only
/// proposes; the user edits the result pane and saves.
pub fn resolve_conflict(
    tab: &Entity<RepoTab>,
    path: BString,
    operation: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(repo) = repo_of(tab, cx) else { return };
    let shown = String::from_utf8_lossy(&path).into_owned();
    ask_with(
        tab,
        format!("Resolve the conflict in {shown}"),
        async move {
            let bytes = repo
                .read_worktree_file(&path)
                .await
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "This file is not UTF-8 text; resolve it by hand.".to_string())?;
            let text = ai::event::truncate(&text, ai::prompts::MAX_DIFF);
            Ok(ai::prompts::resolve_conflict(&shown, &operation, &text))
        },
        window,
        cx,
    );
}

/// "Summarise PR": hook for the pull-request panel. `diff` is the PR's unified diff (it is cut
/// to fit).
#[allow(dead_code)]
pub fn summarise_pr(
    tab: &Entity<RepoTab>,
    number: u64,
    title: String,
    description: String,
    diff: String,
    window: &mut Window,
    cx: &mut App,
) {
    ask_with(
        tab,
        format!("Summarise PR #{number}: {title}"),
        async move {
            let diff = ai::event::truncate(&diff, ai::prompts::MAX_DIFF);
            Ok(ai::prompts::summarise_pr(&title, &description, &diff))
        },
        window,
        cx,
    );
}

/// "Write commit message" for the staged changes: `(summary, body)`. Uses the ACP adapter when
/// one is found (one-shot session, tools denied), else the Claude API with the stored key.
/// Never commits.
pub fn write_commit_message(
    tab: &Entity<RepoTab>,
    cx: &mut App,
) -> Task<Result<(String, String), String>> {
    let Some(repo) = repo_of(tab, cx) else {
        return Task::ready(Err("The repository is not open yet.".into()));
    };
    let branch = tab
        .read(cx)
        .status
        .as_ref()
        .and_then(|s| s.branch.head.clone());
    let root = super::root_of(tab, cx);
    let settings = super::hub(cx).read(cx).settings.clone();
    cx.background_executor().spawn(async move {
        let files = repo
            .diff(DiffTarget::Staged { paths: Vec::new() })
            .await
            .map_err(|e| e.to_string())?;
        if files.is_empty() {
            return Err("Stage some changes first.".into());
        }
        let diff = ai::prompts::diff_text(&files, ai::prompts::MAX_DIFF);
        let text = match ai::adapter::find(&settings.lookup()) {
            Ok(cmd) => {
                let prompt = ai::prompts::commit_message_for_agent(branch.as_deref(), &diff);
                ai::one_shot(cmd, root, prompt).await?
            }
            Err(_) => match ai::claude_api::load_key()? {
                Some(key) => {
                    ai::claude_api::complete(
                        key,
                        settings.model(),
                        ai::prompts::COMMIT_SYSTEM.to_string(),
                        ai::prompts::commit_message(branch.as_deref(), &diff),
                    )
                    .await?
                }
                None => {
                    return Err(
                        "No AI available: install Claude Code's ACP adapter (see the \
                                Chat view) or add a Claude API key in Settings \u{2192} AI."
                            .into(),
                    );
                }
            },
        };
        let (summary, body) = ai::prompts::split_message(&text);
        if summary.is_empty() {
            return Err("The AI returned an empty message.".into());
        }
        Ok((summary, body))
    })
}
