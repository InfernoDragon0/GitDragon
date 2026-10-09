//! App-wide actions and their default key bindings.

use gpui_kit::{Action, App, KeyBinding, actions};

actions!(
    gitdragon,
    [
        Quit,
        ShowGraph,
        ShowChanges,
        ShowConflicts,
        ShowPullRequests,
        ShowIssues,
        ShowBoards,
        ShowChat,
        ShowSettings,
        ToggleLeftDock,
        ToggleRightDock,
        ToggleDarkMode,
        NextSkin,
        OpenRepository,
        CloneRepository,
        NewRepository,
        CloseTab,
        NextTab,
        PreviousTab,
        Refresh,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        CommandPalette,
        UndoLast,
    ]
);

/// `secondary` is Cmd on macOS and Ctrl elsewhere.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-1", ShowGraph, None),
        KeyBinding::new("secondary-2", ShowChanges, None),
        KeyBinding::new("secondary-3", ShowConflicts, None),
        KeyBinding::new("secondary-4", ShowPullRequests, None),
        KeyBinding::new("secondary-5", ShowIssues, None),
        KeyBinding::new("secondary-6", ShowBoards, None),
        KeyBinding::new("secondary-shift-a", ShowChat, None),
        KeyBinding::new("secondary-,", ShowSettings, None),
        KeyBinding::new("secondary-b", ToggleLeftDock, None),
        KeyBinding::new("secondary-alt-b", ToggleRightDock, None),
        KeyBinding::new("secondary-shift-l", ToggleDarkMode, None),
        KeyBinding::new("secondary-shift-k", NextSkin, None),
        KeyBinding::new("secondary-o", OpenRepository, None),
        KeyBinding::new("secondary-shift-o", CloneRepository, None),
        KeyBinding::new("secondary-shift-n", NewRepository, None),
        KeyBinding::new("secondary-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("f5", Refresh, None),
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ZoomReset, None),
        KeyBinding::new("secondary-shift-p", CommandPalette, None),
        KeyBinding::new("secondary-alt-z", UndoLast, None),
    ]);
}

/// Every bound action with the name the Shortcuts list shows. The keys themselves are asked of
/// the window (`bindings_for_action`), so the list always shows what is really bound.
pub fn shortcut_actions() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Command palette", Box::new(CommandPalette)),
        ("Open settings", Box::new(ShowSettings)),
        ("Show graph", Box::new(ShowGraph)),
        ("Show changes", Box::new(ShowChanges)),
        ("Show conflicts", Box::new(ShowConflicts)),
        ("Show pull / merge requests", Box::new(ShowPullRequests)),
        ("Show issues", Box::new(ShowIssues)),
        ("Show boards", Box::new(ShowBoards)),
        ("Show AI chat", Box::new(ShowChat)),
        ("Refresh", Box::new(Refresh)),
        ("Undo last operation", Box::new(UndoLast)),
        ("Open repository", Box::new(OpenRepository)),
        ("Clone repository", Box::new(CloneRepository)),
        ("New repository", Box::new(NewRepository)),
        ("Close tab", Box::new(CloseTab)),
        ("Next tab", Box::new(NextTab)),
        ("Previous tab", Box::new(PreviousTab)),
        ("Toggle sidebar", Box::new(ToggleLeftDock)),
        ("Toggle details", Box::new(ToggleRightDock)),
        ("Light / dark mode", Box::new(ToggleDarkMode)),
        ("Switch theme", Box::new(NextSkin)),
        ("Zoom in", Box::new(ZoomIn)),
        ("Zoom out", Box::new(ZoomOut)),
        ("Reset zoom", Box::new(ZoomReset)),
        ("Quit", Box::new(Quit)),
    ]
}

/// `ctrl-shift-p` as people write it: `Ctrl+Shift+P`.
pub fn pretty_keys(unparsed: &str) -> String {
    let mut rest = unparsed;
    let mut out: Vec<String> = Vec::new();
    loop {
        let modifier = [
            ("ctrl-", "Ctrl"),
            ("alt-", "Alt"),
            ("shift-", "Shift"),
            ("cmd-", "Cmd"),
            ("win-", "Win"),
            ("super-", "Super"),
            ("fn-", "Fn"),
        ]
        .into_iter()
        .find(|(prefix, _)| rest.len() > prefix.len() && rest.starts_with(prefix));
        match modifier {
            Some((prefix, name)) => {
                out.push(name.to_string());
                rest = &rest[prefix.len()..];
            }
            None => break,
        }
    }
    let mut key = rest.chars();
    out.push(match key.next() {
        Some(first) => first.to_uppercase().collect::<String>() + key.as_str(),
        None => String::new(),
    });
    out.join("+")
}

/// The keys bound to `action`, e.g. `Ctrl+Shift+P`; `None` when unbound.
pub fn keys_for(window: &gpui_kit::Window, action: &dyn Action) -> Option<String> {
    let binding = window.bindings_for_action(action).into_iter().next_back()?;
    let keys: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|k| pretty_keys(&k.unparse()))
        .collect();
    (!keys.is_empty()).then(|| keys.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_like_labels() {
        assert_eq!(pretty_keys("ctrl-shift-p"), "Ctrl+Shift+P");
        assert_eq!(pretty_keys("ctrl-alt-z"), "Ctrl+Alt+Z");
        assert_eq!(pretty_keys("f5"), "F5");
        assert_eq!(pretty_keys("ctrl-tab"), "Ctrl+Tab");
        assert_eq!(pretty_keys("ctrl-,"), "Ctrl+,");
        assert_eq!(pretty_keys("ctrl--"), "Ctrl+-");
        assert_eq!(pretty_keys("cmd-shift-p"), "Cmd+Shift+P");
        assert_eq!(pretty_keys(""), "");
    }

    #[test]
    fn shortcut_list_has_unique_names() {
        let mut names: Vec<_> = shortcut_actions().iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
