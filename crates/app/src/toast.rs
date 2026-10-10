//! Toasts (gpui-kit notifications) for results the user did not look at: a failed operation,
//! an Undo on offer. Messages pass through `git_core::redact` first, so a token that ended up in
//! a git error never reaches the screen.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, ElementId, SharedString, Window};

use crate::repo_tab::RepoTab;

/// Marker for the id of Undo toasts (one per tab and record).
struct UndoToast;

/// Longest message shown; git can print pages.
const MAX_CHARS: usize = 400;
const MAX_LINES: usize = 6;

/// Runs `f` on the main window after the current update has finished (a notification cannot be
/// pushed from inside an update of that window).
fn on_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App) + 'static) {
    cx.defer(move |cx| {
        let handle = cx.active_window().or_else(|| cx.windows().first().copied());
        if let Some(handle) = handle {
            _ = handle.update(cx, |_, window, cx| f(window, cx));
        }
    });
}

/// The text a toast shows for `message`: redacted, at most a few lines.
pub fn clean(message: &str) -> String {
    let redacted = git_core::redact::redact(message);
    let redacted = ui::sanitize_display(&redacted);
    let mut out = String::new();
    for (i, line) in redacted
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
    {
        if i == MAX_LINES {
            out.push_str("\n…");
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect::<String>() + "…";
    }
    out
}

/// A failed operation (stays until closed: errors should be read).
pub fn error(cx: &mut App, title: impl Into<SharedString>, message: &str) {
    let note = Notification::error(clean(message))
        .title(title)
        .autohide(false);
    on_window(cx, move |window, cx| window.push_notification(note, cx));
}

/// A short confirmation (hides itself).
#[allow(
    dead_code,
    reason = "used by callers that confirm quietly finished work"
)]
pub fn info(cx: &mut App, message: impl Into<SharedString>) {
    let note = Notification::info(message);
    on_window(cx, move |window, cx| window.push_notification(note, cx));
}

/// "<operation> done" with an Undo button; [`dismiss_undo`] removes it when the offer ends.
pub fn undo(cx: &mut App, tab: gpui_kit::WeakEntity<RepoTab>, id: u64, message: String) {
    on_window(cx, move |window, cx| {
        let note = Notification::success(message)
            .id1::<UndoToast>(ElementId::Integer(id))
            .action(move |_, _, _| {
                let tab = tab.clone();
                Button::new("toast-undo")
                    .primary()
                    .label("Undo")
                    .on_click(move |_, window, cx| {
                        _ = tab.update(cx, |tab, cx| tab.undo_last(cx));
                        window.remove_notification1::<UndoToast>(ElementId::Integer(id), cx);
                    })
            });
        window.push_notification(note, cx);
    });
}

/// Removes the Undo toast of record `id` (undone, expired or replaced).
pub fn dismiss_undo(cx: &mut App, id: u64) {
    on_window(cx, move |window, cx| {
        window.remove_notification1::<UndoToast>(ElementId::Integer(id), cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_redacted_and_trimmed() {
        let long = "x".repeat(MAX_CHARS * 2);
        let cleaned = clean(&long);
        assert!(cleaned.chars().count() <= MAX_CHARS + 1);
        assert!(cleaned.ends_with('…'));

        let many = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = clean(&many);
        assert_eq!(cleaned.lines().count(), MAX_LINES + 1);
        assert!(cleaned.ends_with('…'));
        assert_eq!(clean("\n\nfatal: oops\n\n"), "fatal: oops");
    }

    #[test]
    fn urls_with_credentials_are_masked() {
        let cleaned = clean("fatal: unable to access 'https://user:s3cretpass@example.com/r.git/'");
        assert!(!cleaned.contains("s3cretpass"), "{cleaned}");
    }
}
