//! The working copy's changes, reached through the graph's WIP row: the commit box at the top of
//! the details panel ([`commit_box`]), and the staging diff with hunk / line staging that opens
//! in place of the graph when a WIP file is clicked ([`diff`]). Every git call goes through the
//! tab's one-at-a-time operation ([`RepoTab::run_op`]), so the operation strip shows it and its
//! Cancel stops it; the file watcher refreshes the tab afterwards.

pub mod commit_box;
pub mod diff;
pub mod section;

use bstr::{BString, ByteSlice as _};
use git_core::parse::status::StatusEntry;
use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    App, ElementId, Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use ui::Palette;

use crate::repo_tab::RepoTab;
pub use section::Section;

/// Paths an operation on `entry` must name (both sides of a rename).
fn op_paths(entry: &StatusEntry) -> Vec<BString> {
    let mut paths = vec![entry.path.clone()];
    paths.extend(entry.orig_path.clone());
    paths
}

/// Stages (`stage`) or unstages `entries`; an empty list means every change.
pub fn stage(tab: &Entity<RepoTab>, entries: &[StatusEntry], stage: bool, cx: &mut App) {
    let paths: Vec<BString> = entries.iter().flat_map(op_paths).collect();
    let label = match (stage, entries) {
        (true, [one]) => format!("Staging {}", one.path.to_str_lossy()),
        (false, [one]) => format!("Unstaging {}", one.path.to_str_lossy()),
        (true, _) => "Staging".to_string(),
        (false, _) => "Unstaging".to_string(),
    };
    tab.update(cx, |tab, cx| {
        tab.run_op(label, cx, move |repo| async move {
            match (stage, paths.is_empty()) {
                (true, true) => repo.stage_all().await,
                (true, false) => repo.stage(&paths).await,
                (false, true) => repo.unstage_all().await,
                (false, false) => repo.unstage(&paths).await,
            }
        })
    });
}

/// Asks first, then throws away unstaged changes (tracked) or deletes files (untracked).
pub fn confirm_discard(
    tab: &Entity<RepoTab>,
    section: Section,
    entries: Vec<StatusEntry>,
    window: &mut Window,
    cx: &mut App,
) {
    if entries.is_empty() || !matches!(section, Section::Unstaged | Section::Untracked) {
        return;
    }
    let what = match entries.as_slice() {
        [one] => format!("\u{201c}{}\u{201d}", one.path.to_str_lossy()),
        many => format!("{} files", many.len()),
    };
    let untracked = section == Section::Untracked;
    let (title, body, ok) = if untracked {
        (
            "Delete untracked files?",
            format!("{what} will be deleted from disk. This cannot be undone."),
            "Delete",
        )
    } else {
        (
            "Discard changes?",
            format!("Unstaged changes to {what} will be lost. This cannot be undone."),
            "Discard",
        )
    };
    let tab = tab.downgrade();
    crate::dialogs::confirm(window, cx, title, body, ok, true, move |_, cx| {
        let Some(tab) = tab.upgrade() else { return };
        let paths: Vec<BString> = entries.iter().map(|e| e.path.clone()).collect();
        tab.update(cx, |tab, cx| {
            if untracked {
                tab.run_op("Deleting untracked files", cx, move |repo| async move {
                    repo.delete_untracked(&paths).await
                })
            } else {
                tab.run_op("Discarding changes", cx, move |repo| async move {
                    repo.discard(&paths).await
                })
            }
        });
    });
}

/// Stashes every change, untracked files included, as `WIP on <branch>`.
pub fn stash(tab: &Entity<RepoTab>, cx: &mut App) {
    tab.update(cx, |tab, cx| {
        let branch = tab
            .status
            .as_ref()
            .and_then(|s| s.branch.head.clone())
            .unwrap_or_else(|| "detached HEAD".into());
        let message = format!("WIP on {branch}");
        tab.run_op("Stashing", cx, move |repo| async move {
            repo.stash_push(&message, true, false, &[]).await
        })
    });
}

/// Applies the latest stash and drops it.
pub fn stash_pop(tab: &Entity<RepoTab>, cx: &mut App) {
    tab.update(cx, |tab, cx| {
        tab.run_op("Popping stash@{0}", cx, |repo| async move {
            repo.stash_action(git_core::StashAction::Pop, 0).await
        })
    });
}

/// "LFS" chip on a file stored in Git LFS.
pub fn lfs_badge(palette: &Palette) -> impl IntoElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .text_xs()
        .text_color(palette.accent)
        .bg(palette.accent.opacity(0.14))
        .child("LFS")
}

/// Lock chip: green when the lock is yours, warning colour with the owner's name otherwise.
pub fn lock_badge(
    id: ElementId,
    ours: bool,
    owner: SharedString,
    palette: &Palette,
) -> impl IntoElement {
    let color = if ours { palette.ok } else { palette.warn };
    let tip: SharedString = if ours {
        "Locked by you".into()
    } else {
        format!("Locked by {owner}: you cannot push changes to it").into()
    };
    h_flex()
        .id(id)
        .flex_none()
        .gap_0p5()
        .px_1()
        .rounded_sm()
        .text_xs()
        .text_color(color)
        .bg(color.opacity(0.14))
        .child(Icon::new(IconName::Lock).xsmall())
        .when(!ours, |d| d.child(owner))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
}
