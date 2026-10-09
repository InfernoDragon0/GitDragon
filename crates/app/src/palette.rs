//! Command palette (Ctrl+Shift+P): a fuzzy list of everything the menus and the rail can do.
//! Rows that run git show the command line they will run. Choosing a row hands the
//! [`Command`] to the workspace, which runs it through the same code as the buttons and menus.

use std::rc::Rc;

use git_core::PullMode;
use gpui_kit::component::list::{List, ListDelegate, ListItem, ListState};
use gpui_kit::component::{IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    Action, App, AppContext as _, Context, IntoElement, ParentElement as _, SharedString,
    Styled as _, Task, Window, div, px,
};
use hosts::HostVocab;
use ui::Palette;

use crate::actions;
use crate::rail::View;
use crate::settings_find::Section;

/// What a palette row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ShowView(View),
    ShowChanges,
    SettingsSection(Section),
    Refresh,
    Fetch,
    Pull(PullMode),
    /// The Pull button's own mode (Settings > Pull & push).
    PullDefault,
    Push,
    Stash,
    PopStash,
    NewBranch,
    MergeBranch,
    RebaseOnto,
    UndoLast,
    OpenRepository,
    CloneRepository,
    NewRepository,
    CloseTab,
    ToggleLeftDock,
    ToggleRightDock,
    ToggleMode,
    NextSkin,
    ZoomIn,
    ZoomOut,
    ZoomReset,
}

/// What the palette needs to know to list only what works right now and to word it for the
/// active repository.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub repo: bool,
    pub branch: Option<String>,
    pub upstream: bool,
    pub remote: Option<String>,
    pub stashes: usize,
    pub vocab: Option<&'static HostVocab>,
    /// The Undo on offer: what it does, and the command it runs.
    pub undo: Option<(String, String)>,
    pub pull_default: Option<PullMode>,
    pub prune: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub command: Command,
    pub title: String,
    /// The git command line it runs (user-typed parts as `<placeholders>`), if it runs git.
    pub git: Option<String>,
}

fn entry(command: Command, title: impl Into<String>, git: Option<String>) -> Entry {
    Entry {
        command,
        title: title.into(),
        git,
    }
}

/// Every command that makes sense in `ctx`, in the order they list with an empty query.
pub fn catalog(ctx: &Ctx) -> Vec<Entry> {
    let mut out = Vec::new();
    let vocab = ctx.vocab.unwrap_or(&HostVocab::GENERIC);
    if ctx.repo {
        out.push(entry(Command::ShowView(View::Graph), "Go to Graph", None));
        out.push(entry(Command::ShowChanges, "Go to Changes", None));
        out.push(entry(
            Command::ShowView(View::Conflicts),
            "Go to Conflicts",
            None,
        ));
        out.push(entry(
            Command::ShowView(View::PullRequests),
            format!("Go to {}", vocab.pull_requests),
            None,
        ));
        out.push(entry(
            Command::ShowView(View::Issues),
            format!("Go to {}", vocab.issues),
            None,
        ));
        out.push(entry(Command::ShowView(View::Boards), "Go to Boards", None));
        out.push(entry(Command::ShowView(View::Chat), "Go to AI chat", None));

        out.push(entry(Command::Refresh, "Refresh this repository", None));
        out.push(entry(
            Command::Fetch,
            "Fetch all remotes",
            Some(git_core::sync_ops::fetch_all_preview(ctx.prune)),
        ));
        let default = ctx.pull_default.unwrap_or(PullMode::Config);
        if ctx.upstream {
            out.push(entry(
                Command::PullDefault,
                "Pull (Settings default)",
                Some(git_core::sync_ops::pull_preview(default)),
            ));
            for (mode, title) in [
                (PullMode::Config, "Pull, per git config"),
                (PullMode::Merge, "Pull, merge"),
                (PullMode::Rebase, "Pull, rebase"),
                (PullMode::FastForwardOnly, "Pull, fast-forward only"),
            ] {
                out.push(entry(
                    Command::Pull(mode),
                    title,
                    Some(git_core::sync_ops::pull_preview(mode)),
                ));
            }
        }
        if let Some(branch) = &ctx.branch {
            let git = if ctx.upstream {
                Some("git push".to_string())
            } else {
                ctx.remote.as_ref().map(|remote| {
                    format!(
                        "git push --no-progress --set-upstream --end-of-options {remote} \
                         refs/heads/{branch}:refs/heads/{branch}"
                    )
                })
            };
            out.push(entry(Command::Push, format!("Push {branch}"), git));
        }
        out.push(entry(
            Command::Stash,
            "Stash all changes…",
            Some("git stash push --quiet --include-untracked --message=<message>".into()),
        ));
        if ctx.stashes > 0 {
            out.push(entry(
                Command::PopStash,
                "Pop the latest stash",
                Some("git stash pop --quiet --index stash@{0}".into()),
            ));
        }
        out.push(entry(
            Command::NewBranch,
            "New branch from HEAD…",
            Some("git branch --end-of-options <name> HEAD".into()),
        ));
        out.push(entry(
            Command::MergeBranch,
            "Merge a branch into the current one…",
            Some("git merge --no-edit --end-of-options <branch>".into()),
        ));
        out.push(entry(
            Command::RebaseOnto,
            "Rebase the current branch onto…",
            Some("git rebase --end-of-options <branch>".into()),
        ));
        if let Some((what, git)) = &ctx.undo {
            out.push(entry(
                Command::UndoLast,
                format!("Undo: {what}"),
                Some(git.clone()),
            ));
        }
        out.push(entry(Command::CloseTab, "Close this repository", None));
    }
    out.push(entry(Command::OpenRepository, "Open a repository…", None));
    out.push(entry(Command::CloneRepository, "Clone a repository…", None));
    out.push(entry(Command::NewRepository, "Create a repository…", None));
    out.push(entry(
        Command::ShowView(View::Settings),
        "Open Settings",
        None,
    ));
    for section in Section::ALL {
        out.push(entry(
            Command::SettingsSection(section),
            format!("Settings: {}", section.title()),
            None,
        ));
    }
    out.push(entry(Command::ToggleLeftDock, "Toggle the sidebar", None));
    out.push(entry(
        Command::ToggleRightDock,
        "Toggle the details panel",
        None,
    ));
    out.push(entry(Command::ToggleMode, "Switch light / dark mode", None));
    out.push(entry(Command::NextSkin, "Switch theme", None));
    out.push(entry(Command::ZoomIn, "Zoom in", None));
    out.push(entry(Command::ZoomOut, "Zoom out", None));
    out.push(entry(Command::ZoomReset, "Reset zoom", None));
    out
}

/// The bound action behind a command, for showing its shortcut.
fn action_for(command: Command) -> Option<Box<dyn Action>> {
    use crate::actions::*;
    Some(match command {
        Command::ShowView(View::Graph) => Box::new(ShowGraph),
        Command::ShowView(View::Conflicts) => Box::new(ShowConflicts),
        Command::ShowView(View::PullRequests) => Box::new(ShowPullRequests),
        Command::ShowView(View::Issues) => Box::new(ShowIssues),
        Command::ShowView(View::Boards) => Box::new(ShowBoards),
        Command::ShowView(View::Chat) => Box::new(ShowChat),
        Command::ShowView(View::Settings) => Box::new(ShowSettings),
        Command::ShowChanges => Box::new(ShowChanges),
        Command::Refresh => Box::new(Refresh),
        Command::UndoLast => Box::new(UndoLast),
        Command::OpenRepository => Box::new(OpenRepository),
        Command::CloneRepository => Box::new(CloneRepository),
        Command::NewRepository => Box::new(NewRepository),
        Command::CloseTab => Box::new(CloseTab),
        Command::ToggleLeftDock => Box::new(ToggleLeftDock),
        Command::ToggleRightDock => Box::new(ToggleRightDock),
        Command::ToggleMode => Box::new(ToggleDarkMode),
        Command::NextSkin => Box::new(NextSkin),
        Command::ZoomIn => Box::new(ZoomIn),
        Command::ZoomOut => Box::new(ZoomOut),
        Command::ZoomReset => Box::new(ZoomReset),
        _ => return None,
    })
}

// ---- fuzzy matching ---------------------------------------------------------------------------

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// How well one word matches `hay`, or `None`. A contiguous match beats a scattered one, a
/// match at a word start beats one in the middle, and earlier beats later.
fn word_score(word: &[char], hay: &[char]) -> Option<i32> {
    let lowered: Vec<char> = hay.iter().map(|c| lower(*c)).collect();
    let boundary = |i: usize| i == 0 || !hay[i - 1].is_alphanumeric();
    if let Some(start) = lowered.windows(word.len()).position(|w| w == word) {
        let mut score = 100 - start.min(60) as i32;
        if boundary(start) {
            score += 20;
        }
        if start == 0 {
            score += 10;
        }
        return Some(score);
    }
    // Scattered: the letters in order. Rewards runs and word starts, charges for gaps.
    let mut score = 0;
    let mut at = 0;
    let mut last: Option<usize> = None;
    for &c in word {
        let found = (at..lowered.len()).find(|&i| lowered[i] == c)?;
        score += 2;
        if boundary(found) {
            score += 8;
        }
        match last {
            Some(prev) if found == prev + 1 => score += 5,
            Some(prev) => score -= (found - prev - 1).min(6) as i32,
            None => score -= found.min(6) as i32,
        }
        last = Some(found);
        at = found + 1;
    }
    Some(score.clamp(1, 60))
}

/// Score of `query` (words separated by spaces, all must match, any case) against `hay`;
/// `None` when a word is missing. Higher is better.
pub fn fuzzy_score(query: &str, hay: &str) -> Option<i32> {
    let hay: Vec<char> = hay.chars().collect();
    let mut total = 0;
    let mut any = false;
    for word in query.split_whitespace() {
        let word: Vec<char> = word.chars().map(lower).collect();
        total += word_score(&word, &hay)?;
        any = true;
    }
    any.then_some(total).or(Some(0))
}

/// Indices into `entries`, best match first (catalog order for ties and for an empty query).
pub fn rank(entries: &[Entry], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(i32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(ix, e)| {
            let title = fuzzy_score(query, &e.title).map(|s| s * 2);
            let git = e.git.as_deref().and_then(|g| fuzzy_score(query, g));
            Some((title.into_iter().chain(git).max()?, ix))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

// ---- the list -----------------------------------------------------------------------------------

type Run = Rc<dyn Fn(Command, &mut Window, &mut App)>;

struct Delegate {
    entries: Vec<Entry>,
    shortcuts: Vec<Option<String>>,
    shown: Vec<usize>,
    selected: Option<usize>,
    run: Run,
}

impl ListDelegate for Delegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.shown = rank(&self.entries, query);
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.shown.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let at = *self.shown.get(ix.row)?;
        let entry = &self.entries[at];
        let palette = Palette::of(cx);
        let shortcut = self.shortcuts[at].clone();
        Some(
            ListItem::new(ix.row).h(px(40.)).px_3().child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(palette.emph)
                                    .truncate()
                                    .child(SharedString::from(entry.title.clone())),
                            )
                            .children(entry.git.clone().map(|git| {
                                div()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .truncate()
                                    .child(SharedString::from(git))
                            })),
                    )
                    .children(shortcut.map(|keys| {
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(palette.border)
                            .text_xs()
                            .text_color(palette.muted)
                            .child(SharedString::from(keys))
                    })),
            ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let palette = Palette::of(cx);
        div()
            .p_4()
            .text_sm()
            .text_color(palette.muted)
            .child("No command matches.")
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix.map(|i| i.row);
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(command) = self
            .selected
            .and_then(|row| self.shown.get(row))
            .map(|at| self.entries[*at].command)
        else {
            return;
        };
        let run = self.run.clone();
        window.close_dialog(cx);
        // Commands may open dialogs of their own: let this one finish closing first.
        window.defer(cx, move |window, cx| run(command, window, cx));
    }
}

/// Opens the palette as a dialog. `run` gets the chosen command.
pub fn open(
    ctx: &Ctx,
    run: impl Fn(Command, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let entries = catalog(ctx);
    let shortcuts = entries
        .iter()
        .map(|e| action_for(e.command).and_then(|a| actions::keys_for(window, a.as_ref())))
        .collect();
    let shown = (0..entries.len()).collect();
    let delegate = Delegate {
        entries,
        shortcuts,
        shown,
        selected: None,
        run: Rc::new(run),
    };
    let state = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
    state.update(cx, |list, cx| {
        list.set_selected_index(Some(IndexPath::default()), window, cx);
        list.focus(window, cx);
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Command palette").w(px(640.)).child(
            List::new(&state)
                .search_placeholder("Type a command, e.g. \"pull rebase\"")
                .h(px(400.)),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            repo: true,
            branch: Some("main".into()),
            upstream: true,
            remote: Some("origin".into()),
            stashes: 1,
            vocab: Some(&HostVocab::GITLAB),
            undo: None,
            pull_default: Some(PullMode::Rebase),
            prune: true,
        }
    }

    fn titles(entries: &[Entry], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| entries[i].title.clone()).collect()
    }

    #[test]
    fn fuzzy_matches_subsequences_in_any_case() {
        assert!(fuzzy_score("psh", "Push main").is_some());
        assert!(fuzzy_score("PUSH", "push main").is_some());
        assert!(fuzzy_score("hsup", "Push main").is_none());
        assert!(fuzzy_score("xyz", "Push main").is_none());
        // Every word has to be there, in any order.
        assert!(fuzzy_score("main push", "Push main").is_some());
        assert!(fuzzy_score("push nope", "Push main").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn contiguous_and_early_and_word_start_matches_win() {
        let prefix = fuzzy_score("pull", "Pull, rebase").unwrap();
        let later = fuzzy_score("pull", "Fetch then pull").unwrap();
        let scattered = fuzzy_score("pull", "Pop the latest stash, then unlock").unwrap_or(0);
        assert!(prefix > later, "{prefix} {later}");
        assert!(later > scattered, "{later} {scattered}");
        let mid_word = fuzzy_score("ull", "Pull").unwrap();
        let word_start = fuzzy_score("ull", "Ullman").unwrap();
        assert!(word_start > mid_word);
    }

    #[test]
    fn ranking_puts_the_best_title_first_and_keeps_catalog_order_for_empty_queries() {
        let entries = catalog(&ctx());
        let all = rank(&entries, "");
        assert_eq!(all, (0..entries.len()).collect::<Vec<_>>());
        assert_eq!(all, rank(&entries, "   "));

        let fetch = rank(&entries, "fet");
        assert_eq!(entries[fetch[0]].command, Command::Fetch);

        let rebase = rank(&entries, "rebase");
        let top = titles(&entries, &rebase[..2]);
        assert!(
            top.iter().any(|t| t.starts_with("Rebase the current")),
            "{top:?}"
        );
        assert!(rank(&entries, "zzzzqq").is_empty());
    }

    #[test]
    fn git_command_text_is_searchable() {
        let entries = catalog(&ctx());
        let hits = rank(&entries, "--ff-only");
        assert!(
            hits.iter()
                .any(|&i| entries[i].command == Command::Pull(PullMode::FastForwardOnly))
        );
    }

    #[test]
    fn catalog_follows_the_repo_and_host() {
        let entries = catalog(&ctx());
        assert!(entries.iter().any(|e| e.title == "Go to Merge requests"));
        assert!(entries.iter().all(|e| e.title != "Go to Pull requests"));

        let none = catalog(&Ctx::default());
        assert!(none.iter().all(|e| e.command != Command::Fetch));
        assert!(none.iter().all(|e| e.command != Command::ShowChanges));
        assert!(none.iter().any(|e| e.command == Command::OpenRepository));
        assert!(
            none.iter()
                .any(|e| e.command == Command::ShowView(View::Settings))
        );

        // Pull only with an upstream; Pop only with stashes; Undo only when offered.
        let mut c = ctx();
        c.upstream = false;
        c.stashes = 0;
        let entries = catalog(&c);
        assert!(
            entries
                .iter()
                .all(|e| !matches!(e.command, Command::Pull(_) | Command::PullDefault))
        );
        assert!(entries.iter().all(|e| e.command != Command::PopStash));
        assert!(entries.iter().all(|e| e.command != Command::UndoLast));
        c.undo = Some(("Merging topic".into(), "git reset --keep abc".into()));
        let undo = catalog(&c)
            .into_iter()
            .find(|e| e.command == Command::UndoLast)
            .unwrap();
        assert_eq!(undo.title, "Undo: Merging topic");
        assert_eq!(undo.git.as_deref(), Some("git reset --keep abc"));
    }

    #[test]
    fn previews_show_the_exact_git_command() {
        let entries = catalog(&ctx());
        let git = |command: Command| {
            entries
                .iter()
                .find(|e| e.command == command)
                .and_then(|e| e.git.clone())
        };
        assert_eq!(
            git(Command::Fetch).as_deref(),
            Some("git fetch --no-progress --prune --all")
        );
        assert_eq!(
            git(Command::Pull(PullMode::Rebase)).as_deref(),
            Some("git pull --no-progress --rebase")
        );
        // The Pull button's mode comes from Settings.
        assert_eq!(
            git(Command::PullDefault).as_deref(),
            Some("git pull --no-progress --rebase")
        );
        assert_eq!(git(Command::Push).as_deref(), Some("git push"));
        assert!(git(Command::MergeBranch).unwrap().starts_with("git merge "));
        // Without an upstream the push names the remote and sets tracking.
        let mut c = ctx();
        c.upstream = false;
        let entries = catalog(&c);
        let push = entries.iter().find(|e| e.command == Command::Push).unwrap();
        assert_eq!(
            push.git.as_deref(),
            Some(
                "git push --no-progress --set-upstream --end-of-options origin \
                 refs/heads/main:refs/heads/main"
            )
        );
        // Commands that do not run git say nothing.
        assert!(
            entries
                .iter()
                .filter(|e| matches!(e.command, Command::ShowView(_) | Command::ZoomIn))
                .all(|e| e.git.is_none())
        );
    }

    #[test]
    fn every_settings_section_has_a_command_and_titles_are_unique() {
        let entries = catalog(&ctx());
        for section in Section::ALL {
            assert!(
                entries
                    .iter()
                    .any(|e| e.command == Command::SettingsSection(section)),
                "{section:?}"
            );
        }
        let mut titles: Vec<_> = entries.iter().map(|e| e.title.as_str()).collect();
        let total = titles.len();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), total);
    }
}
