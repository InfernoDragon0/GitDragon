//! The Changes tab of a pull request page: the files it changes, then one file's diff.
//!
//! Loading goes in steps, each painted as it lands, none on the UI thread: the head (or the
//! host's merge preview) is fetched into `refs/gitdragon/pr/<n>/...` with the user's git
//! (reused while the head sha is unchanged, see `git_core::pr_ops`), the file list follows
//! (names first, line counts after), and a file's diff is read when it is picked. When the
//! fetch can't happen (no repository, a fork the remote can't see, sign-in failure, offline)
//! the host's API lists the files with their patches instead, labelled as possibly
//! truncated. Errors stay inside the tab.

use std::ops::Range;
use std::sync::Arc;

use ::hosts::{Feature, GitHost, PrFile, PrFileStatus, PullRequest, RepoId};
use bstr::ByteSlice as _;
use git_core::parse::diff::FileChange;
use git_core::pr_ops::{PrFetch, PrRange};
use git_core::{ChangedFile, CliRepo, FailureKind, GitError};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClickEvent, Context, ElementId, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, ListHorizontalSizingBehavior, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle,
    Window, div, prelude::FluentBuilder as _, uniform_list,
};
use ui::diff::copy::{self as diff_copy, RowSelection};
use ui::diff::{self as diff_ui, DiffModel, Layout};
use ui::{Palette, zoomed};

use crate::diff_select::{self, DiffSelect};
use crate::hosted::Session;

/// Height of one file row.
const FILE_ROW: f32 = 26.;
/// File rows shown before the list scrolls.
const FILE_ROWS_SHOWN: usize = 10;
/// The diff box is the window height less this (page header, tabs, status, a margin)...
const DIFF_CHROME: f32 = 230.;
/// ...but never shorter than this.
const DIFF_MIN: f32 = 320.;

/// Where the file list came from.
#[derive(Clone)]
enum Source {
    Git {
        repo: CliRepo,
        range: PrRange,
    },
    /// The host's API; `why` says why git could not be used.
    Api {
        truncated: bool,
        why: SharedString,
    },
}

/// How one file's diff is read.
#[derive(Clone)]
enum FileData {
    Git(ChangedFile),
    /// The API's hunks; `None` when it sent none (binary, too large).
    Api(Option<Arc<str>>),
}

struct FileRow {
    path: SharedString,
    /// Before a rename or copy.
    old_path: Option<SharedString>,
    change: FileChange,
    /// `None` until counted, and for binary files.
    added: Option<u32>,
    removed: Option<u32>,
    data: FileData,
}

enum Phase {
    /// Nothing asked yet.
    Idle,
    /// A step is running; the text says which.
    Loading(SharedString),
    Ready,
    Failed(SharedString),
}

struct OpenDiff {
    ix: usize,
    /// `None` while it is read and highlighted.
    model: Option<Arc<DiffModel>>,
    error: Option<SharedString>,
    _task: Option<Task<()>>,
}

/// What [`ChangesView::show`] needs of the page.
pub struct ChangesRequest<'a> {
    pub repo: Option<CliRepo>,
    pub session: &'a Session,
    pub pr: &'a PullRequest,
    /// `PR` / `MR`.
    pub short: &'static str,
}

pub struct ChangesView {
    /// The pull request and head sha shown.
    key: Option<(u64, String)>,
    /// To ask again (Retry, manual refresh).
    last: Option<Pending>,
    phase: Phase,
    source: Option<Source>,
    files: Arc<Vec<FileRow>>,
    open: Option<OpenDiff>,
    layout: Layout,
    sel: RowSelection,
    focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
}

/// Everything a load needs, owned.
#[derive(Clone)]
struct Pending {
    repo: Option<CliRepo>,
    host: Arc<dyn GitHost>,
    repo_id: RepoId,
    remote: String,
    number: u64,
    head_sha: String,
    base_sha: Option<String>,
    target: String,
    short: &'static str,
}

impl ChangesView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            key: None,
            last: None,
            phase: Phase::Idle,
            source: None,
            files: Arc::new(Vec::new()),
            open: None,
            layout: Layout::Unified,
            sel: RowSelection::default(),
            focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            task: None,
        }
    }

    /// Starts loading `pr`'s changes unless they are shown (or loading) for its head already.
    pub fn show(&mut self, req: ChangesRequest<'_>, cx: &mut Context<Self>) {
        let key = (req.pr.number, req.pr.head_sha.clone());
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key);
        self.start(
            Pending {
                repo: req.repo,
                host: req.session.host.clone(),
                repo_id: req.session.repo.clone(),
                remote: req.session.remote.clone(),
                number: req.pr.number,
                head_sha: req.pr.head_sha.clone(),
                base_sha: req.pr.base_sha.clone(),
                target: req.pr.target_branch.clone(),
                short: req.short,
            },
            cx,
        );
    }

    /// The page now shows another pull request (or head): drop what was loaded for the old one
    /// and stop its fetch, without starting a new one (the Changes tab is not shown).
    pub fn retarget(&mut self, number: u64, head_sha: &str) {
        if self
            .key
            .as_ref()
            .is_some_and(|(n, s)| *n != number || s != head_sha)
        {
            self.hide();
        }
    }

    /// The page closed: stop loading; the next `show` starts over.
    pub fn hide(&mut self) {
        self.key = None;
        self.task = None;
    }

    /// Loads the shown pull request again (Retry, a manual refresh). The fetched ref is still
    /// reused while the head is unchanged.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.last.clone()
            && self.key.is_some()
        {
            self.start(pending, cx);
        }
    }

    /// Files listed so far, for the tab label.
    pub fn file_count(&self) -> Option<usize> {
        matches!(self.phase, Phase::Ready).then_some(self.files.len())
    }

    fn start(&mut self, p: Pending, cx: &mut Context<Self>) {
        self.last = Some(p.clone());
        self.source = None;
        self.files = Arc::new(Vec::new());
        self.open = None;
        self.sel.clear();
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.phase = Phase::Loading("Loading changes\u{2026}".into());
        self.task = Some(cx.spawn(async move |this, cx| {
            let bg = cx.background_executor().clone();
            let pull_ref = p.host.pull_changes_ref(p.number);
            let git: Result<(CliRepo, PrRange), String> = match (p.repo.clone(), pull_ref) {
                (None, _) => Err("the repository is not open".into()),
                (_, None) => Err("the host has no ref to fetch".into()),
                (Some(repo), Some(pull_ref)) => {
                    let preview = pull_ref.merge_preview;
                    let cached = {
                        let (repo, sha, base) =
                            (repo.clone(), p.head_sha.clone(), p.base_sha.clone());
                        bg.spawn(async move {
                            repo.cached_pr_range(p.number, &sha, preview, base.as_deref())
                                .await
                        })
                        .await
                    };
                    match cached {
                        Ok(Some(range)) => Ok((repo, range)),
                        _ => {
                            let what = if preview {
                                format!("Fetching the {} merge preview\u{2026}", p.short)
                            } else {
                                format!("Fetching the {} head\u{2026}", p.short)
                            };
                            if this
                                .update(cx, |this, cx| {
                                    this.phase = Phase::Loading(what.into());
                                    cx.notify();
                                })
                                .is_err()
                            {
                                return;
                            }
                            let (remote, target) = (p.remote.clone(), p.target.clone());
                            let (number, base_sha) = (p.number, p.base_sha.clone());
                            bg.spawn(async move {
                                let fetch = PrFetch {
                                    remote: &remote,
                                    source_ref: &pull_ref.reference,
                                    target_branch: &target,
                                    number,
                                    merge_preview: preview,
                                    base_sha: base_sha.as_deref(),
                                };
                                repo.fetch_pr(&fetch).await.map(|range| (repo, range))
                            })
                            .await
                            .map_err(|e| describe(&e))
                        }
                    }
                }
            };
            let why = match git {
                Ok((repo, range)) => {
                    if this
                        .update(cx, |this, cx| {
                            this.phase = Phase::Loading("Listing files\u{2026}".into());
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    let list = |counts: bool| {
                        let (repo, range) = (repo.clone(), range.clone());
                        bg.spawn(
                            async move { repo.range_files(&range.base, &range.head, counts).await },
                        )
                    };
                    match list(false).await {
                        Ok(files) => {
                            let source = Source::Git {
                                repo: repo.clone(),
                                range: range.clone(),
                            };
                            if this
                                .update(cx, |this, cx| this.listed(source, git_rows(files), cx))
                                .is_err()
                            {
                                return;
                            }
                            // Line counts read every blob: they follow the names.
                            if let Ok(files) = list(true).await {
                                _ = this.update(cx, |this, cx| this.counted(git_rows(files), cx));
                            }
                            return;
                        }
                        Err(err) => describe(&err),
                    }
                }
                Err(why) => why,
            };
            // Git could not do it: the host's API.
            if !p.host.supports(Feature::PrFiles) {
                let text = format!(
                    "Could not fetch the {} ({why}), and {} lists no files through its API.",
                    p.short,
                    p.host.vocab().service
                );
                _ = this.update(cx, |this, cx| {
                    this.phase = Phase::Failed(text.into());
                    cx.notify();
                });
                return;
            }
            if this
                .update(cx, |this, cx| {
                    this.phase =
                        Phase::Loading("Loading the file list from the API\u{2026}".into());
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            let answer = p.host.pr_files(&p.repo_id, p.number).await;
            _ = this.update(cx, |this, cx| match answer {
                Ok(listed) => {
                    let source = Source::Api {
                        truncated: listed.truncated,
                        why: why.into(),
                    };
                    this.listed(source, api_rows(listed.files), cx);
                }
                Err(err) => {
                    let text = format!(
                        "Could not fetch the {} ({why}) or list its files through the API: {err}",
                        p.short
                    );
                    this.phase = Phase::Failed(text.into());
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn listed(&mut self, source: Source, rows: Vec<FileRow>, cx: &mut Context<Self>) {
        self.source = Some(source);
        self.files = Arc::new(rows);
        self.phase = Phase::Ready;
        cx.notify();
    }

    /// The same list with line counts (same command, same order).
    fn counted(&mut self, rows: Vec<FileRow>, cx: &mut Context<Self>) {
        if rows.len() == self.files.len() {
            self.files = Arc::new(rows);
            cx.notify();
        }
    }

    /// Opens file `ix`'s diff; it is read and highlighted off the UI thread.
    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.files.get(ix) else {
            return;
        };
        window.focus(&self.focus, cx);
        if self.open.as_ref().is_some_and(|o| o.ix == ix) {
            return;
        }
        self.sel.clear();
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        let bg = cx.background_executor().clone();
        let job = match (&row.data, &self.source) {
            (FileData::Git(file), Some(Source::Git { repo, range })) => {
                let (repo, range, file) = (repo.clone(), range.clone(), file.clone());
                bg.spawn(async move {
                    match repo.range_file_diff(&range.base, &range.head, &file).await {
                        Ok(Some(diff)) => Ok(DiffModel::new(diff)),
                        Ok(None) => Err("No changes in this file.".to_string()),
                        Err(err) => Err(format!("Could not read the diff: {}", describe(&err))),
                    }
                })
            }
            (FileData::Api(Some(patch)), _) => {
                let patch = patch.clone();
                let (old, new, change) = (
                    row.old_path.clone().unwrap_or_else(|| row.path.clone()),
                    row.path.clone(),
                    row.change,
                );
                bg.spawn(async move {
                    let old = (change != FileChange::Added).then_some(old.as_bytes());
                    let new = (change != FileChange::Deleted).then_some(new.as_bytes());
                    git_core::parse::diff::from_hunks(old, new, change, patch.as_bytes())
                        .map(DiffModel::new)
                        .map_err(|err| format!("Could not read the API's patch: {err}"))
                })
            }
            (FileData::Api(None), _) => {
                self.open = Some(OpenDiff {
                    ix,
                    model: None,
                    error: Some(
                        "The API sent no diff for this file (binary, or too large).".into(),
                    ),
                    _task: None,
                });
                cx.notify();
                return;
            }
            (FileData::Git(_), _) => return,
        };
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |this, cx| {
                if let Some(open) = this.open.as_mut().filter(|o| o.ix == ix) {
                    match result {
                        Ok(model) => open.model = Some(Arc::new(model)),
                        Err(text) => open.error = Some(text.into()),
                    }
                    cx.notify();
                }
            });
        });
        self.open = Some(OpenDiff {
            ix,
            model: None,
            error: None,
            _task: Some(task),
        });
        cx.notify();
    }

    fn toggle_layout(&mut self, cx: &mut Context<Self>) {
        self.layout = match self.layout {
            Layout::Unified => Layout::Split,
            Layout::Split => Layout::Unified,
        };
        // Row indices differ between the layouts.
        self.sel.clear();
        cx.notify();
    }
}

/// A git failure in a few words.
fn describe(err: &GitError) -> String {
    match err {
        GitError::Failed {
            kind, stderr_tail, ..
        } => match kind {
            FailureKind::AuthFailed => "sign-in was refused".into(),
            FailureKind::Network => "the remote could not be reached".into(),
            FailureKind::BadRevision => "the remote has no such ref".into(),
            _ => {
                let last = stderr_tail.lines().last().unwrap_or("git failed");
                let last = last.trim().trim_start_matches("fatal: ");
                if last.contains("couldn't find remote ref") {
                    "the remote has no such ref (a fork it can't see?)".into()
                } else {
                    ui::sanitize_line(last).into_owned()
                }
            }
        },
        GitError::Timeout { .. } => "the fetch timed out".into(),
        GitError::InvalidArgument { what, .. } => format!("invalid {what}"),
        other => ui::sanitize_line(&other.to_string()).into_owned(),
    }
}

fn git_rows(files: Vec<ChangedFile>) -> Vec<FileRow> {
    files
        .into_iter()
        .map(|f| {
            let lossy = |p: &bstr::BString| SharedString::from(p.to_str_lossy().into_owned());
            let path = SharedString::from(f.path().to_str_lossy().into_owned());
            let old_path = match f.change {
                FileChange::Renamed { .. } | FileChange::Copied { .. } => {
                    f.old_path.as_ref().map(lossy)
                }
                _ => None,
            };
            FileRow {
                path,
                old_path,
                change: f.change,
                added: f.added,
                removed: f.removed,
                data: FileData::Git(f),
            }
        })
        .collect()
}

fn api_rows(files: Vec<PrFile>) -> Vec<FileRow> {
    files
        .into_iter()
        .map(|f| {
            let change = match f.status {
                PrFileStatus::Added => FileChange::Added,
                PrFileStatus::Removed => FileChange::Deleted,
                PrFileStatus::Modified => FileChange::Modified,
                PrFileStatus::Renamed => FileChange::Renamed { similarity: 0 },
                PrFileStatus::Copied => FileChange::Copied { similarity: 0 },
            };
            FileRow {
                path: f.path.into(),
                old_path: f.old_path.map(Into::into),
                change,
                added: Some(f.additions),
                removed: Some(f.deletions),
                data: FileData::Api(f.patch.map(Arc::from)),
            }
        })
        .collect()
}

fn change_letter(change: FileChange, palette: &Palette) -> (&'static str, Hsla) {
    match change {
        FileChange::Added => ("A", palette.ok),
        FileChange::Deleted => ("D", palette.error),
        FileChange::Modified => ("M", palette.warn),
        FileChange::Renamed { .. } => ("R", palette.accent),
        FileChange::Copied { .. } => ("C", palette.accent),
        FileChange::ModeChanged => ("T", palette.muted),
    }
}

// ---- rendering ------------------------------------------------------------------------------

impl Render for ChangesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let body = match &self.phase {
            Phase::Idle => message("Loading changes\u{2026}", &palette),
            Phase::Loading(text) => h_flex()
                .gap_2()
                .p_3()
                .text_sm()
                .text_color(palette.muted)
                .child(div().child(text.clone()))
                .into_any_element(),
            Phase::Failed(text) => v_flex()
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(palette.error.opacity(0.4))
                .bg(palette.error.opacity(0.08))
                .child(
                    div()
                        .text_sm()
                        .text_color(palette.error)
                        .child(SharedString::from(ui::sanitize_display(text).into_owned())),
                )
                .child(
                    div().child(
                        Button::new("pr-changes-retry")
                            .xsmall()
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("Try again")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reload(cx))),
                    ),
                )
                .into_any_element(),
            Phase::Ready if self.files.is_empty() => {
                let same = matches!(&self.source,
                    Some(Source::Git { range, .. }) if range.base == range.head);
                message(
                    if same {
                        "No changes between base and head"
                    } else {
                        "No file changes between the target and the head."
                    },
                    &palette,
                )
            }
            Phase::Ready => {
                let viewport = window.viewport_size().height;
                let diff_height = (viewport - zoomed(DIFF_CHROME, cx)).max(zoomed(DIFF_MIN, cx));
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(self.render_summary(&palette, cx))
                    .child(self.render_list(&palette, cx))
                    .child(
                        div()
                            .w_full()
                            .h(diff_height)
                            .child(self.render_diff(&palette, cx)),
                    )
                    .into_any_element()
            }
        };
        v_flex().id("pr-changes").w_full().gap_2().child(body)
    }
}

fn message(text: &'static str, palette: &Palette) -> AnyElement {
    div()
        .p_3()
        .text_sm()
        .text_color(palette.muted)
        .child(text)
        .into_any_element()
}

impl ChangesView {
    /// Totals and where the list came from.
    fn render_summary(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.files.len();
        let (added, removed) = self.files.iter().fold((0u64, 0u64), |(a, r), f| {
            (
                a + u64::from(f.added.unwrap_or(0)),
                r + u64::from(f.removed.unwrap_or(0)),
            )
        });
        let counted = self.files.iter().any(|f| f.added.is_some());
        let (origin, tip, color): (String, Option<SharedString>, Hsla) = match &self.source {
            Some(Source::Git { range, .. }) if range.merge_preview => (
                format!("Merge preview {}", short_sha(&range.head)),
                Some("The host's preview of the merge into the target branch: its changes against the target.".into()),
                palette.muted,
            ),
            Some(Source::Git { range, .. }) => (
                format!("{}..{}", short_sha(&range.base), short_sha(&range.head)),
                Some("Fetched with git: merge base of the target branch..head.".into()),
                palette.muted,
            ),
            Some(Source::Api { truncated, why }) => (
                if *truncated {
                    "From the API (truncated)".to_string()
                } else {
                    "From the API (may be truncated)".to_string()
                },
                Some(format!("Git could not fetch it: {why}").into()),
                palette.warn,
            ),
            None => (String::new(), None, palette.muted),
        };
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .text_xs()
            .child(
                div()
                    .text_color(palette.fg)
                    .child(SharedString::from(format!(
                        "{count} file{}",
                        if count == 1 { "" } else { "s" }
                    ))),
            )
            .when(counted, |d| {
                d.child(div().text_color(palette.ok).child(format!("+{added}")))
                    .child(
                        div()
                            .text_color(palette.error)
                            .child(format!("\u{2212}{removed}")),
                    )
            })
            .child(div().flex_1())
            .child(
                div()
                    .id("pr-changes-origin")
                    .text_color(color)
                    .child(SharedString::from(origin))
                    .when_some(tip, |d, tip| {
                        d.tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                .build(window, cx)
                        })
                    }),
            )
            .child(
                Button::new("pr-changes-reload")
                    .xsmall()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip("Load the changes again")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reload(cx))),
            )
            .into_any_element()
    }

    /// The virtualised file list.
    fn render_list(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.files.len().clamp(1, FILE_ROWS_SHOWN);
        v_flex()
            .relative()
            .w_full()
            .h(zoomed(FILE_ROW * rows as f32 + 2., cx))
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel2)
            .child(
                uniform_list(
                    "pr-changes-files",
                    self.files.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
                .track_scroll(&self.list_scroll)
                .pr_3()
                .size_full(),
            )
            .vertical_scrollbar(&self.list_scroll)
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = Palette::of(cx);
        let open = self.open.as_ref().map(|o| o.ix);
        let files = self.files.clone();
        let counted = self.counted_any();
        range
            .filter_map(|ix| {
                let f = files.get(ix)?;
                let (letter, color) = change_letter(f.change, &palette);
                let (dir, name) = match f.path.rsplit_once('/') {
                    Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                    None => (String::new(), f.path.to_string()),
                };
                let selected = open == Some(ix);
                let from = f.old_path.as_ref().map(|old| format!("\u{2190} {old}"));
                Some(
                    h_flex()
                        .id(ElementId::NamedInteger("pr-changes-file".into(), ix as u64))
                        .h(zoomed(FILE_ROW, cx))
                        .w_full()
                        .px_2()
                        .gap_1p5()
                        .text_xs()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(palette.accent_soft))
                        .when(!selected, |d| d.hover(|s| s.bg(palette.hover)))
                        .child(
                            div()
                                .w(zoomed(12., cx))
                                .flex_none()
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .text_color(color)
                                .child(letter),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .child(div().flex_none().text_color(palette.emph).child(
                                    SharedString::from(ui::sanitize_line(&name).into_owned()),
                                ))
                                .child(
                                    div()
                                        .min_w_0()
                                        .pl_1()
                                        .truncate()
                                        .text_color(palette.muted)
                                        .child(SharedString::from(
                                            ui::sanitize_line(&dir).into_owned(),
                                        )),
                                )
                                .when_some(from, |d, from| {
                                    d.child(
                                        div()
                                            .min_w_0()
                                            .pl_1()
                                            .truncate()
                                            .text_color(palette.muted)
                                            .child(SharedString::from(
                                                ui::sanitize_line(&from).into_owned(),
                                            )),
                                    )
                                }),
                        )
                        .when(counted && f.added.is_none(), |d| {
                            d.child(div().flex_none().text_color(palette.muted).child("bin"))
                        })
                        .when(f.added.is_some_and(|n| n > 0), |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_color(palette.ok)
                                    .child(format!("+{}", f.added.unwrap_or(0))),
                            )
                        })
                        .when(f.removed.is_some_and(|n| n > 0), |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_color(palette.error)
                                    .child(format!("\u{2212}{}", f.removed.unwrap_or(0))),
                            )
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.select(ix, window, cx)
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// Line counts have arrived (git) or came with the list (API).
    fn counted_any(&self) -> bool {
        self.files.iter().any(|f| f.added.is_some())
    }

    /// The picked file's diff: header (path, counts, Unified / Split) over the rows.
    fn render_diff(&self, palette: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = &self.open else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(palette.border)
                .text_sm()
                .text_color(palette.muted)
                .child("Select a file to see its diff.")
                .into_any_element();
        };
        let layout = self.layout;
        let path = self
            .files
            .get(open.ix)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let (added, removed) = open.model.as_ref().map_or((0, 0), |m| (m.added, m.removed));
        let header = h_flex()
            .flex_none()
            .h(zoomed(34., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(palette.emph)
                    .child(SharedString::from(ui::sanitize_line(&path).into_owned())),
            )
            .when(open.model.is_some(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(palette.ok)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.error)
                        .child(format!("\u{2212}{removed}")),
                )
            })
            .child(
                Button::new("pr-changes-layout")
                    .ghost()
                    .xsmall()
                    .label(match layout {
                        Layout::Unified => "Split",
                        Layout::Split => "Unified",
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_layout(cx))),
            );
        let note = |text: SharedString, color: Hsla| {
            div()
                .p_4()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let body = match (&open.model, &open.error) {
            (_, Some(error)) => note(error.clone(), palette.muted),
            (None, None) => note("Loading diff\u{2026}".into(), palette.muted),
            (Some(model), _) if model.lfs.is_some() => model.lfs.as_ref().map_or_else(
                || note("".into(), palette.muted),
                |c| diff_ui::lfs_summary(c, palette, cx),
            ),
            (Some(model), _) if model.file.binary => note("Binary file.".into(), palette.muted),
            (Some(model), _) if model.rows(layout).is_empty() => {
                note("No content changes.".into(), palette.muted)
            }
            (Some(model), _) => {
                let model = model.clone();
                let widest = diff_ui::widest_row(&model, layout);
                let view = cx.entity().downgrade();
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "pr-changes-diff",
                            model.rows(layout).len(),
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                let palette = Palette::of(cx);
                                range
                                    .filter_map(|ix| {
                                        let selected = this.sel.contains(ix);
                                        let row = diff_ui::read_only_row(
                                            &model,
                                            this.layout,
                                            ix,
                                            selected,
                                            &palette,
                                            cx,
                                        )?;
                                        Some(diff_select::row(ix, row, cx))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.diff_scroll)
                        // Long lines scroll sideways: the list is as wide as its widest row.
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .with_width_from_item(widest)
                        .pr_3()
                        .pb_3()
                        .flex_1(),
                    )
                    .scrollbar(&self.diff_scroll, ScrollbarAxis::Both)
                    .context_menu(move |menu, _, _| diff_select::menu(menu, view.clone()))
                    .into_any_element()
            }
        };
        diff_select::container(v_flex().size_full(), &self.focus, cx)
            .rounded_md()
            .border_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .overflow_hidden()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

impl DiffSelect for ChangesView {
    fn selection(&mut self) -> &mut RowSelection {
        &mut self.sel
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn copy_selection(&mut self, patch: bool, cx: &mut Context<Self>) {
        let (Some(rows), Some(model)) = (
            self.sel.range(),
            self.open.as_ref().and_then(|o| o.model.as_ref()),
        ) else {
            return;
        };
        let text = if patch {
            diff_copy::copy_patch(model, self.layout, rows)
        } else {
            diff_copy::copy_lines(model, self.layout, rows)
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
    }
}
