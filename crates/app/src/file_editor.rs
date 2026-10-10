//! The in-app file editor: one worktree file in gpui-kit's code editor (line numbers, undo,
//! search), opened at a line from a diff row or the Conflicts list. Save writes the file with its
//! original line endings; a file that is not UTF-8 text is refused and offered to an external
//! editor instead. The host view (staging diff, Conflicts) shows it in place of its own content
//! and keeps unsaved editors around when the user navigates elsewhere ([`Editors`]).

use std::path::{Path, PathBuf};

use bstr::{BString, ByteSlice as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor as EditorInput, EditorState, InputEvent, Position};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _,
};
use ui::{Palette, zoomed};

use crate::conflicts::external::{self, Editor};
use crate::dialogs;
use crate::repo_tab::RepoTab;

/// Larger files open in an external editor.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

// ---- pure logic -----------------------------------------------------------------------------

/// How the file on disk ends its lines. The editor itself works in `\n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

/// A file's text as the editor holds it (`\n` endings) plus what is needed to write it back.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    pub ending: LineEnding,
    /// Both `\r\n` and bare `\n` lines were present: saving makes them all `ending`.
    pub mixed: bool,
}

/// Why a file is not opened in the editor.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Binary,
    TooLarge,
    NotUtf8,
    Symlink,
    NotAFile,
    Io(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Binary => "This is a binary file.".into(),
            Refusal::TooLarge => "This file is too large for the built-in editor.".into(),
            Refusal::NotUtf8 => {
                "This file is not UTF-8 text, so saving it here could corrupt it.".into()
            }
            Refusal::Symlink => {
                "This path is a symbolic link; the editor does not follow links.".into()
            }
            Refusal::NotAFile => "This path is not a regular file.".into(),
            Refusal::Io(err) => err.clone(),
        }
    }
}

/// Splits `bytes` into the editor's text and the line ending to restore on save.
pub fn decode(bytes: &[u8]) -> Result<Decoded, Refusal> {
    if bytes.len() > MAX_BYTES {
        return Err(Refusal::TooLarge);
    }
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err(Refusal::Binary);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Refusal::NotUtf8)?;
    let crlf = bytes.windows(2).filter(|w| w == b"\r\n").count();
    let lf = bytes.iter().filter(|&&b| b == b'\n').count();
    let ending = if crlf > 0 && crlf * 2 > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    Ok(Decoded {
        text: if crlf > 0 {
            text.replace("\r\n", "\n")
        } else {
            text.to_owned()
        },
        ending,
        mixed: crlf != 0 && crlf != lf,
    })
}

/// The bytes written for the editor's `text`.
pub fn encode(text: &str, ending: LineEnding) -> Vec<u8> {
    match ending {
        LineEnding::Lf => text.as_bytes().to_vec(),
        // Normalise first so a pasted `\r\n` does not become `\r\r\n`.
        LineEnding::Crlf => text
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .into_bytes(),
    }
}

/// The highlight language (a gpui-component grammar name) for `path`, from its file name or
/// extension. Names without a compiled-in grammar render as plain text.
pub fn language_for(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.to_ascii_lowercase().as_str() {
        "makefile" | "gnumakefile" => return Some("make"),
        "cmakelists.txt" => return Some("cmake"),
        _ => {}
    }
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None; // `.gitignore`: a dotfile, not an extension
    }
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" => "python",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "cs" => "csharp",
        "go" => "go",
        "json" | "jsonc" => "json",
        "yml" | "yaml" => "yaml",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sh" | "bash" => "bash",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "sql" => "sql",
        "swift" => "swift",
        "zig" => "zig",
        "proto" => "proto",
        "svelte" => "svelte",
        "cmake" => "cmake",
        _ => return None,
    })
}

/// The worktree file `rel` (a repo-relative path from git) under `root`; refuses paths that are
/// absolute or climb out of the worktree.
pub fn target_path(root: &Path, rel: &[u8]) -> Result<PathBuf, Refusal> {
    git_core::ops::worktree_file(root, rel).map_err(|e| match e {
        git_core::GitError::InvalidArgument { what, .. } if what == git_core::ops::LINKED_PATH => {
            Refusal::Symlink
        }
        e => Refusal::Io(e.to_string()),
    })
}

/// The 1-based line a diff line stands for in the new file: its own, else the nearest one after
/// (a deleted line sits where the next kept line is), else the nearest before. `lines` are the
/// new-side numbers of a hunk's lines in order.
pub fn editor_line(lines: &[Option<u32>], ix: usize) -> Option<u32> {
    let own = lines.get(ix).copied().flatten();
    own.or_else(|| lines.iter().skip(ix + 1).find_map(|n| *n))
        .or_else(|| lines.iter().take(ix).rev().find_map(|n| *n))
}

/// Reads `target` for the editor. Refuses links, non-files, binary, huge and non-UTF-8 files.
pub fn read_file(target: &Path) -> Result<(Vec<u8>, Decoded), Refusal> {
    let meta = std::fs::symlink_metadata(target).map_err(|e| Refusal::Io(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(Refusal::Symlink);
    }
    if !meta.is_file() {
        return Err(Refusal::NotAFile);
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err(Refusal::TooLarge);
    }
    let bytes = std::fs::read(target).map_err(|e| Refusal::Io(e.to_string()))?;
    let decoded = decode(&bytes)?;
    Ok((bytes, decoded))
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    /// The file no longer holds what was loaded (another program saved or removed it).
    ChangedOnDisk,
}

/// Writes `bytes` to `target`. With `expect`, only when the file still holds exactly that.
pub fn write_file(
    target: &Path,
    bytes: &[u8],
    expect: Option<&[u8]>,
) -> Result<SaveOutcome, Refusal> {
    let meta = std::fs::symlink_metadata(target);
    if meta.as_ref().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Refusal::Symlink);
    }
    if let Some(expect) = expect {
        match std::fs::read(target) {
            Ok(now) if now == expect => {}
            _ => return Ok(SaveOutcome::ChangedOnDisk),
        }
    }
    std::fs::write(target, bytes).map_err(|e| Refusal::Io(e.to_string()))?;
    Ok(SaveOutcome::Saved)
}

// ---- the view -------------------------------------------------------------------------------

pub enum FileEditorEvent {
    /// The editor was closed (saved, discarded or never edited): the host shows itself again.
    Close,
}

enum Load {
    Loading,
    Ready,
    Refused(SharedString),
}

pub struct FileEditor {
    tab: Entity<RepoTab>,
    path: BString,
    back_label: &'static str,
    load: Load,
    editor: Entity<EditorState>,
    /// The editor's text as loaded or last saved.
    saved: String,
    /// The bytes the file held then (to notice another program changing it).
    on_disk: Vec<u8>,
    ending: LineEnding,
    mixed: bool,
    dirty: bool,
    saving: bool,
    /// 1-based line to put the cursor on once loaded.
    line: Option<u32>,
    externals: Vec<(Editor, PathBuf)>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FileEditorEvent> for FileEditor {}

impl FileEditor {
    pub fn new(
        tab: Entity<RepoTab>,
        path: BString,
        line: Option<u32>,
        back_label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        let mut _subs = vec![cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_change(cx);
            }
        })];
        // The custom editor from Settings can change while this view is open.
        if let Some(prefs) = crate::prefs::handle(cx) {
            _subs.push(cx.observe(&prefs, |this, _, cx| {
                this.externals =
                    external::available(crate::prefs::get(cx).external_editor.as_deref());
                cx.notify();
            }));
        }
        let mut view = Self {
            tab,
            path,
            back_label,
            load: Load::Loading,
            editor,
            saved: String::new(),
            on_disk: Vec::new(),
            ending: LineEnding::Lf,
            mixed: false,
            dirty: false,
            saving: false,
            line,
            externals: external::available(crate::prefs::get(cx).external_editor.as_deref()),
            error: None,
            task: None,
            _subs,
        };
        view.load(window, cx);
        view
    }

    pub fn path(&self) -> &BString {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Moves the cursor to the 1-based `line` (and focuses the editor).
    pub fn go_to(&mut self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.line = Some(line);
        if matches!(self.load, Load::Ready) {
            self.editor.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(line.saturating_sub(1), 0), window, cx)
            });
        }
    }

    fn root(&self, cx: &App) -> Option<PathBuf> {
        let repo = self.tab.read(cx).repo.clone()?;
        use git_core::GitRepo as _;
        Some(repo.root().to_path_buf())
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            self.load = Load::Refused("The repository is not open.".into());
            return;
        };
        let rel = self.path.clone();
        self.load = Load::Loading;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_file(&target_path(&root, &rel)?) })
                .await;
            _ = this.update_in(cx, |view, window, cx| view.loaded(result, window, cx));
        }));
        cx.notify();
    }

    fn loaded(
        &mut self,
        result: Result<(Vec<u8>, Decoded), Refusal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (bytes, decoded) = match result {
            Ok(loaded) => loaded,
            Err(refusal) => {
                self.load = Load::Refused(refusal.message().into());
                cx.notify();
                return;
            }
        };
        let language = language_for(&self.path.to_str_lossy());
        let position = self.line.map(|l| Position::new(l.saturating_sub(1), 0));
        self.saved = decoded.text.clone();
        self.on_disk = bytes;
        self.ending = decoded.ending;
        self.mixed = decoded.mixed;
        self.dirty = false;
        self.load = Load::Ready;
        self.editor.update(cx, |state, cx| {
            if let Some(language) = language {
                state.set_highlighter(language, cx);
            }
            state.set_value(decoded.text, window, cx);
            if let Some(position) = position {
                state.set_cursor_position(position, window, cx);
            }
        });
        cx.notify();
    }

    fn on_change(&mut self, cx: &mut Context<Self>) {
        let state = self.editor.read(cx);
        let dirty = state.text().len() != self.saved.len() || state.value() != self.saved;
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.notify();
        }
    }

    /// Writes the file. `force` skips the check that nobody else changed it meanwhile.
    fn save(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.load, Load::Ready) || self.saving {
            return;
        }
        let Some(root) = self.root(cx) else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        let bytes = encode(&text, self.ending);
        let expect = (!force).then(|| self.on_disk.clone());
        let rel = self.path.clone();
        self.saving = true;
        self.error = None;
        let written = bytes.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        write_file(&target_path(&root, &rel)?, &bytes, expect.as_deref())
                    })
                    .await;
            _ = this.update_in(cx, |view, window, cx| {
                view.saving = false;
                match result {
                    Ok(SaveOutcome::Saved) => {
                        view.on_disk = written;
                        view.saved = text;
                        view.on_change(cx);
                        view.tab.update(cx, |tab, cx| tab.refresh(cx));
                    }
                    Ok(SaveOutcome::ChangedOnDisk) => view.confirm_overwrite(window, cx),
                    Err(refusal) => view.error = Some(refusal.message().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        dialogs::confirm(
            window,
            cx,
            "File changed on disk",
            format!(
                "{} was changed by another program since you opened it. Saving overwrites those \
                 changes.",
                self.path.to_str_lossy()
            ),
            "Overwrite",
            true,
            move |window, cx| {
                _ = this.update(cx, |view, cx| view.save(true, window, cx));
            },
        );
    }

    /// Closes after asking when there are unsaved changes.
    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty {
            cx.emit(FileEditorEvent::Close);
            return;
        }
        let this = cx.entity().downgrade();
        dialogs::confirm(
            window,
            cx,
            "Discard unsaved changes?",
            format!(
                "{} has changes that are not saved.",
                self.path.to_str_lossy()
            ),
            "Discard",
            true,
            move |_, cx| {
                _ = this.update(cx, |_, cx| cx.emit(FileEditorEvent::Close));
            },
        );
    }

    fn open_external(&mut self, editor: Editor, cx: &mut Context<Self>) {
        let Some(program) = self
            .externals
            .iter()
            .find(|(e, _)| *e == editor)
            .map(|(_, p)| p.clone())
        else {
            return;
        };
        let (Some(root), rel) = (self.root(cx), self.path.clone()) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let target = target_path(&root, &rel)?;
                    let args = external::args(editor, &target, None)
                        .ok_or_else(|| Refusal::Io("path is not absolute".into()))?;
                    external::launch(&program, &args).map_err(|e| Refusal::Io(e.to_string()))
                })
                .await;
            _ = this.update(cx, |view, cx| {
                view.error = result.err().map(|e| e.message().into());
                cx.notify();
            });
        });
        task.detach();
    }
}

impl Focusable for FileEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl FileEditor {
    fn render_header(&self, palette: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(self.load, Load::Ready);
        h_flex()
            .flex_none()
            .h(zoomed(36., cx))
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                Button::new("file-editor-back")
                    .ghost()
                    .small()
                    .label(self.back_label)
                    .tooltip("Close the editor")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.request_close(window, cx)
                    })),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(palette.emph)
                    .child(SharedString::from(self.path.to_str_lossy().into_owned())),
            )
            .when(self.dirty, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(palette.warn)
                        .child("\u{25cf} modified"),
                )
            })
            .when(ready, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(self.ending.label()),
                )
            })
            .child(div().flex_1())
            .children(self.externals.iter().map(|(editor, program)| {
                let editor = *editor;
                Button::new(SharedString::from(format!("edit-open-{editor:?}")))
                    .xsmall()
                    .ghost()
                    .label(external::label(editor, program))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_external(editor, cx)
                    }))
            }))
            .child(
                Button::new("file-editor-save")
                    .xsmall()
                    .primary()
                    .label("Save")
                    .tooltip("Save (Ctrl+S)")
                    .disabled(!ready || !self.dirty || self.saving)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.save(false, window, cx)
                        }),
                    ),
            )
    }

    fn render_body(&self, palette: &Palette) -> AnyElement {
        let message = |text: SharedString, color| {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        match &self.load {
            Load::Loading => message("Loading\u{2026}".into(), palette.muted),
            Load::Refused(text) => message(
                format!("{text} Use one of the editors above to open it.").into(),
                palette.muted,
            ),
            Load::Ready => div()
                .size_full()
                .px_1()
                .child(EditorInput::new(&self.editor).h_full().bordered(false))
                .into_any_element(),
        }
    }
}

impl Render for FileEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let notice = |text: SharedString, color: gpui_kit::Hsla| {
            div()
                .flex_none()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(color)
                .bg(color.opacity(0.1))
                .child(text)
        };
        v_flex()
            .size_full()
            .bg(palette.panel)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.secondary() && event.keystroke.key == "s" {
                    this.save(false, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(self.render_header(&palette, cx))
            .when(self.mixed, |d| {
                d.child(notice(
                    format!(
                        "Mixed line endings: saving makes every line {}.",
                        self.ending.label()
                    )
                    .into(),
                    palette.warn,
                ))
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(notice(error, palette.error))
            })
            .child(div().flex_1().min_h_0().child(self.render_body(&palette)))
    }
}

// ---- hosting --------------------------------------------------------------------------------

/// The editors a host view has open. An editor with unsaved changes survives the host moving to
/// another file or back to its own content, and comes back when its file is opened again.
#[derive(Default)]
pub struct Editors {
    list: Vec<(Entity<FileEditor>, Subscription)>,
    shown: Option<BString>,
}

/// A view that hosts [`Editors`].
pub trait HasEditors: Sized + 'static {
    fn editors(&mut self) -> &mut Editors;

    /// Opens `path` at the 1-based `line` in the editor shown in place of the host's content.
    fn open_editor(
        &mut self,
        tab: &Entity<RepoTab>,
        path: BString,
        line: Option<u32>,
        back_label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `open` needs the context while the editors live in `self`: lend them out meanwhile.
        let mut editors = std::mem::take(self.editors());
        editors.open(tab, path, line, back_label, window, cx);
        *self.editors() = editors;
    }
}

impl Editors {
    fn find(&self, path: &BString, cx: &App) -> Option<Entity<FileEditor>> {
        self.list
            .iter()
            .find(|(e, _)| e.read(cx).path() == path)
            .map(|(e, _)| e.clone())
    }

    /// The editor to show instead of the host's content.
    pub fn shown(&self, cx: &App) -> Option<Entity<FileEditor>> {
        self.find(self.shown.as_ref()?, cx)
    }

    /// Opens `path` at the 1-based `line`, reusing its editor when there is one.
    fn open<V: HasEditors>(
        &mut self,
        tab: &Entity<RepoTab>,
        path: BString,
        line: Option<u32>,
        back_label: &'static str,
        window: &mut Window,
        cx: &mut Context<V>,
    ) {
        match self.find(&path, cx) {
            Some(editor) => {
                if let Some(line) = line {
                    editor.update(cx, |e, cx| e.go_to(line, window, cx));
                }
            }
            None => {
                let editor = cx.new(|cx| {
                    FileEditor::new(tab.clone(), path.clone(), line, back_label, window, cx)
                });
                let sub = cx.subscribe(&editor, |this: &mut V, editor, _: &FileEditorEvent, cx| {
                    this.editors().remove(&editor, cx);
                    cx.notify();
                });
                self.list.push((editor, sub));
            }
        }
        self.shown = Some(path);
        cx.notify();
    }

    fn remove(&mut self, editor: &Entity<FileEditor>, cx: &App) {
        let closed = editor.read(cx).path().clone();
        self.list.retain(|(e, _)| e != editor);
        if self.shown.as_ref() == Some(&closed) {
            self.shown = None;
        }
    }

    /// The host now deals with `path` (or nothing): show its unsaved editor again, and forget
    /// editors without changes that belong to other files.
    pub fn follow(&mut self, path: Option<&BString>, cx: &App) {
        self.list
            .retain(|(e, _)| e.read(cx).is_dirty() || Some(e.read(cx).path()) == path);
        self.shown = path
            .filter(|p| self.find(p, cx).is_some_and(|e| e.read(cx).is_dirty()))
            .cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lf_files_round_trip() {
        let d = decode(b"a\nb\n").unwrap();
        assert_eq!(d.ending, LineEnding::Lf);
        assert!(!d.mixed);
        assert_eq!(encode(&d.text, d.ending), b"a\nb\n");
    }

    #[test]
    fn crlf_files_keep_crlf_on_save() {
        let d = decode(b"a\r\nb\r\n").unwrap();
        assert_eq!(d.text, "a\nb\n");
        assert_eq!(d.ending, LineEnding::Crlf);
        assert!(!d.mixed);
        assert_eq!(encode(&d.text, d.ending), b"a\r\nb\r\n");
        // An edit that adds a line keeps the style, and a pasted CRLF is not doubled.
        assert_eq!(encode("a\nb\r\nc\n", d.ending), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn mixed_endings_follow_the_majority_and_are_flagged() {
        let d = decode(b"a\r\nb\r\nc\n").unwrap();
        assert_eq!((d.ending, d.mixed), (LineEnding::Crlf, true));
        let d = decode(b"a\nb\nc\r\n").unwrap();
        assert_eq!((d.ending, d.mixed), (LineEnding::Lf, true));
        // A bare `\r` (old Mac) is left alone.
        assert_eq!(decode(b"a\rb").unwrap().text, "a\rb");
    }

    #[test]
    fn no_trailing_newline_is_kept() {
        let d = decode(b"a\r\nb").unwrap();
        assert_eq!(encode(&d.text, d.ending), b"a\r\nb");
        assert_eq!(decode(b"").unwrap().text, "");
    }

    #[test]
    fn unsuitable_files_are_refused() {
        assert_eq!(decode(b"ab\0cd"), Err(Refusal::Binary));
        assert_eq!(decode(&[0x66, 0xff, 0xfe, 0x6f]), Err(Refusal::NotUtf8));
        assert_eq!(decode(&vec![b'a'; MAX_BYTES + 1]), Err(Refusal::TooLarge));
        assert!(decode(&vec![b'a'; MAX_BYTES]).is_ok());
        // UTF-8 (with a BOM) is fine and written back as it came.
        let with_bom = "\u{feff}h\u{e9}llo\n".as_bytes();
        let d = decode(with_bom).unwrap();
        assert_eq!(encode(&d.text, d.ending), with_bom);
    }

    #[test]
    fn languages_come_from_the_extension() {
        assert_eq!(language_for("src/main.rs"), Some("rust"));
        assert_eq!(language_for("a/b/App.TSX"), Some("tsx"));
        assert_eq!(language_for("x.test.js"), Some("javascript"));
        assert_eq!(language_for("config.yml"), Some("yaml"));
        assert_eq!(language_for("Makefile"), Some("make"));
        assert_eq!(language_for("docs/CMakeLists.txt"), Some("cmake"));
        assert_eq!(language_for("notes.txt"), None);
        assert_eq!(language_for("LICENSE"), None);
        // A dotfile has no extension; a dir named like a file type does not count.
        assert_eq!(language_for(".rs"), None);
        assert_eq!(language_for("v1.rs/readme"), None);
    }

    #[test]
    fn paths_must_stay_inside_the_worktree() {
        let root = std::env::temp_dir();
        assert_eq!(
            target_path(&root, b"src/a.rs").unwrap(),
            root.join("src/a.rs")
        );
        for bad in [
            &b"../outside"[..],
            b"a/../../b",
            b"/etc/passwd",
            b"",
            b"a\0b",
        ] {
            assert!(target_path(&root, bad).is_err(), "{bad:?}");
        }
        if cfg!(windows) {
            assert!(target_path(&root, b"C:\\x").is_err());
        }
    }

    #[test]
    fn diff_lines_map_to_editor_lines() {
        // context 10, deleted, deleted, added 11, context 12
        let lines = [Some(10), None, None, Some(11), Some(12)];
        assert_eq!(editor_line(&lines, 0), Some(10));
        assert_eq!(editor_line(&lines, 1), Some(11));
        assert_eq!(editor_line(&lines, 3), Some(11));
        // Only deletions at the end of the hunk: the line before.
        assert_eq!(editor_line(&[Some(4), None], 1), Some(4));
        assert_eq!(editor_line(&[None, None], 0), None);
        assert_eq!(editor_line(&[], 3), None);
    }

    #[test]
    fn read_and_write_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"one\r\ntwo\r\n").unwrap();
        let (bytes, decoded) = read_file(&file).unwrap();
        assert_eq!(decoded.ending, LineEnding::Crlf);

        // Unchanged since loading: written, with the original endings.
        let new = encode("one\nTWO\n", decoded.ending);
        assert_eq!(
            write_file(&file, &new, Some(&bytes)).unwrap(),
            SaveOutcome::Saved
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"one\r\nTWO\r\n");

        // Someone else changed it: refused until forced.
        assert_eq!(
            write_file(&file, b"mine", Some(&bytes)).unwrap(),
            SaveOutcome::ChangedOnDisk
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"one\r\nTWO\r\n");
        assert_eq!(
            write_file(&file, b"mine", None).unwrap(),
            SaveOutcome::Saved
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");

        // Removed since loading counts as changed.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            write_file(&file, b"x", Some(b"mine")).unwrap(),
            SaveOutcome::ChangedOnDisk
        );
    }

    #[test]
    fn non_files_and_non_text_are_refused_on_read() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_file(dir.path()), Err(Refusal::NotAFile));
        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [1, 0, 2]).unwrap();
        assert_eq!(read_file(&bin), Err(Refusal::Binary));
        let latin = dir.path().join("l.txt");
        std::fs::write(&latin, [0x63, 0x61, 0x66, 0xe9]).unwrap();
        assert_eq!(read_file(&latin), Err(Refusal::NotUtf8));
        assert!(matches!(
            read_file(&dir.path().join("gone")),
            Err(Refusal::Io(_))
        ));
    }
}
