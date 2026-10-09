//! App-wide actions and their default key bindings.

use gpui_kit::{App, KeyBinding, actions};

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
    ]);
}
