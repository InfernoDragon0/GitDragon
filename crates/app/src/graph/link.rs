//! What ties the Graph view (centre) to the Details panel (right dock): the selected commit of
//! the active tab, and requests the details panel sends back (open a file's diff, jump to a
//! parent).

use std::sync::Arc;

use git_core::parse::diff::FileDiff;
use gpui_kit::{Context, EventEmitter, WeakEntity};

use crate::repo_tab::RepoTab;

#[derive(Default)]
pub struct GraphLink {
    tab: Option<WeakEntity<RepoTab>>,
    commit: Option<String>,
    /// Path of the file whose diff the graph shows, for the details list highlight.
    open_file: Option<String>,
}

pub enum LinkEvent {
    /// Show this file of `commit` in place of the graph.
    ShowFile {
        tab: WeakEntity<RepoTab>,
        commit: String,
        file: Arc<FileDiff>,
    },
    /// Select (and scroll to) `commit` in the graph.
    Select {
        tab: WeakEntity<RepoTab>,
        commit: String,
    },
}

impl EventEmitter<LinkEvent> for GraphLink {}

impl GraphLink {
    pub fn tab(&self) -> Option<&WeakEntity<RepoTab>> {
        self.tab.as_ref()
    }

    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    pub fn open_file(&self) -> Option<&str> {
        self.open_file.as_deref()
    }

    /// The active tab and its selected commit changed (either may be `None`).
    pub fn set_selection(
        &mut self,
        tab: Option<WeakEntity<RepoTab>>,
        commit: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.tab != tab || self.commit != commit {
            self.tab = tab;
            self.commit = commit;
            self.open_file = None;
            cx.notify();
        }
    }

    pub fn set_open_file(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        if self.open_file != path {
            self.open_file = path;
            cx.notify();
        }
    }
}
