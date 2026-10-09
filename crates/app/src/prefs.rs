//! User preferences from the Settings page, kept in `state.json` (field `prefs`) next to the
//! window layout. Every field has a default so older files load; values that are out of range
//! are pulled back in on load. One shared model: views read it with [`get`], the Settings page
//! writes it with [`update`], and the workspace saves it when it changes.

use std::path::{Path, PathBuf};

use git_core::PullMode;
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use hosts::PollConfig;
use serde::{Deserialize, Serialize};

/// What the toolbar's Pull button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullPref {
    /// Whatever `pull.rebase` / `pull.ff` say.
    #[default]
    Config,
    Merge,
    Rebase,
    FastForwardOnly,
}

impl PullPref {
    pub fn mode(self) -> PullMode {
        match self {
            PullPref::Config => PullMode::Config,
            PullPref::Merge => PullMode::Merge,
            PullPref::Rebase => PullMode::Rebase,
            PullPref::FastForwardOnly => PullMode::FastForwardOnly,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PullPref::Config => "Git config",
            PullPref::Merge => "Merge",
            PullPref::Rebase => "Rebase",
            PullPref::FastForwardOnly => "Fast-forward only",
        }
    }
}

/// Context lines the Diff setting offers.
pub const CONTEXT_CHOICES: [u32; 5] = [1, 3, 5, 10, 20];
/// Host refresh intervals offered (seconds; 0 = only on demand).
pub const REFRESH_CHOICES: [(u32, &str); 5] = [
    (0, "Off"),
    (60, "1 min"),
    (120, "2 min"),
    (300, "5 min"),
    (600, "10 min"),
];
/// Background fetch intervals offered (seconds; 0 = off).
pub const FETCH_CHOICES: [(u32, &str); 5] = [
    (0, "Off"),
    (300, "5 min"),
    (600, "10 min"),
    (1800, "30 min"),
    (3600, "1 hour"),
];
/// Shortest interval honoured for each timer, so a hand-edited file cannot hammer a server.
const MIN_REFRESH_SECS: u32 = 30;
const MIN_FETCH_SECS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Host refresh and background fetch timers.
    pub poll: PollConfig,
    /// Editor for conflicted files, started by this absolute path.
    pub external_editor: Option<PathBuf>,
    /// First branch of repositories created here; empty = git's own default.
    pub default_branch: String,
    /// Show the git command in the operation strip (off: only "Running" / "Idle").
    pub show_command: bool,
    /// Lines of context around each diff hunk.
    pub context_lines: u32,
    /// Ignore whitespace-only changes in committed diffs.
    pub ignore_whitespace: bool,
    /// What the toolbar's Pull button does.
    pub pull: PullPref,
    /// `git fetch --prune` for Fetch and background fetches.
    pub prune_on_fetch: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            poll: PollConfig::default(),
            external_editor: None,
            default_branch: String::new(),
            show_command: true,
            context_lines: 3,
            ignore_whitespace: false,
            pull: PullPref::Config,
            prune_on_fetch: false,
        }
    }
}

impl Prefs {
    /// Pulls values from a hand-edited or newer file back into what the app supports.
    pub fn normalized(mut self) -> Self {
        let range = git_core::parse::diff::CONTEXT_RANGE;
        self.context_lines = self.context_lines.clamp(*range.start(), *range.end());
        let at_least = |secs: u32, min: u32| if secs == 0 { 0 } else { secs.max(min) };
        self.poll.refresh_secs = at_least(self.poll.refresh_secs, MIN_REFRESH_SECS);
        self.poll.fetch_secs = at_least(self.poll.fetch_secs, MIN_FETCH_SECS);
        self.poll.unfocused_factor = self.poll.unfocused_factor.clamp(1, 20);
        if validate_branch_name(&self.default_branch).is_err() {
            self.default_branch.clear();
        }
        if self
            .external_editor
            .as_deref()
            .is_some_and(|p| !p.is_absolute())
        {
            self.external_editor = None;
        }
        self
    }

    /// The branch name for `git init`, if one is set.
    pub fn initial_branch(&self) -> Option<&str> {
        Some(self.default_branch.trim()).filter(|b| !b.is_empty())
    }
}

/// The part of git's ref-name rules that can be checked without a repository (git checks the
/// rest when it runs). Empty is fine: it means "use git's default".
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    let bad = |why: &str| Err(format!("Not a valid branch name: {why}."));
    if name.starts_with('-') {
        return bad("it cannot start with '-'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return bad("slashes must separate names");
    }
    if name.ends_with('.') || name.contains("..") || name.starts_with('.') || name.contains("/.") {
        return bad("no leading dots or '..'");
    }
    if name.ends_with(".lock") || name.contains(".lock/") {
        return bad("it cannot end in .lock");
    }
    if name.contains("@{") || name == "@" {
        return bad("'@{' is reserved");
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(*c))
    {
        return bad(&format!("'{}' is not allowed", c.escape_default()));
    }
    Ok(())
}

/// A custom editor must be an absolute path to an existing file (it is started by that path,
/// never looked up by name).
pub fn validate_editor_path(text: &str) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Enter the full path to the editor's program.".into());
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err("Use the full path, starting from the drive or /.".into());
    }
    if !path.exists() {
        return Err("That file does not exist.".into());
    }
    if !path.is_file() {
        return Err("That is a folder, not a program.".into());
    }
    Ok(path.to_path_buf())
}

/// The shared model behind [`get`] / [`update`].
pub struct PrefsModel {
    prefs: Prefs,
}

/// The preferences changed.
pub struct PrefsChanged;

/// The refresh / fetch timers changed (timers rebuild only for this).
pub struct PollChanged;

impl EventEmitter<PrefsChanged> for PrefsModel {}
impl EventEmitter<PollChanged> for PrefsModel {}

struct PrefsGlobal(Entity<PrefsModel>);

impl Global for PrefsGlobal {}

pub fn init(prefs: Prefs, cx: &mut App) {
    let prefs = prefs.normalized();
    apply(&prefs);
    let model = cx.new(|_| PrefsModel { prefs });
    cx.set_global(PrefsGlobal(model));
}

/// Settings that live outside the app crate (git-core's diff arguments).
fn apply(prefs: &Prefs) {
    git_core::parse::diff::set_view_options(prefs.context_lines, prefs.ignore_whitespace);
}

pub fn handle(cx: &App) -> Option<Entity<PrefsModel>> {
    cx.try_global::<PrefsGlobal>().map(|g| g.0.clone())
}

/// A copy of the current preferences (defaults before [`init`]).
pub fn get(cx: &App) -> Prefs {
    handle(cx).map_or_else(Prefs::default, |m| m.read(cx).prefs.clone())
}

/// Changes the preferences; observers and the workspace's saver are told only when something
/// actually changed.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Prefs)) {
    let Some(model) = handle(cx) else { return };
    model.update(cx, |model, cx| model.change(change, cx));
}

impl PrefsModel {
    fn change(&mut self, change: impl FnOnce(&mut Prefs), cx: &mut Context<Self>) {
        let mut next = self.prefs.clone();
        change(&mut next);
        let next = next.normalized();
        if next != self.prefs {
            apply(&next);
            let poll_changed = next.poll != self.prefs.poll;
            self.prefs = next;
            if poll_changed {
                cx.emit(PollChanged);
            }
            cx.emit(PrefsChanged);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_and_partial_files_load_with_defaults() {
        let empty: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Prefs::default());

        let partial: Prefs =
            serde_json::from_str(r#"{"context_lines": 10, "pull": "rebase", "future": 1}"#)
                .unwrap();
        assert_eq!(partial.context_lines, 10);
        assert_eq!(partial.pull, PullPref::Rebase);
        assert!(partial.show_command);
        assert_eq!(partial.poll, PollConfig::default());

        // A nested PollConfig with one field keeps the others' defaults.
        let poll: Prefs = serde_json::from_str(r#"{"poll": {"fetch_secs": 0}}"#).unwrap();
        assert_eq!(poll.poll.fetch_secs, 0);
        assert_eq!(poll.poll.refresh_secs, PollConfig::default().refresh_secs);
    }

    #[test]
    fn round_trips() {
        let prefs = Prefs {
            external_editor: Some(PathBuf::from("/opt/ed")),
            default_branch: "trunk".into(),
            show_command: false,
            ignore_whitespace: true,
            pull: PullPref::FastForwardOnly,
            prune_on_fetch: true,
            ..Prefs::default()
        };
        let text = serde_json::to_string(&prefs).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&text).unwrap(), prefs);
    }

    #[test]
    fn out_of_range_values_are_pulled_in() {
        let mut prefs = Prefs {
            context_lines: 0,
            default_branch: "bad name".into(),
            external_editor: Some(PathBuf::from("relative/code")),
            ..Prefs::default()
        };
        prefs.poll.refresh_secs = 1;
        prefs.poll.fetch_secs = 5;
        prefs.poll.unfocused_factor = 0;
        let prefs = prefs.normalized();
        assert_eq!(prefs.context_lines, 1);
        assert_eq!(prefs.default_branch, "");
        assert_eq!(prefs.external_editor, None);
        assert_eq!(prefs.poll.refresh_secs, 30);
        assert_eq!(prefs.poll.fetch_secs, 60);
        assert_eq!(prefs.poll.unfocused_factor, 1);

        let mut off = Prefs::default();
        off.poll.refresh_secs = 0;
        off.poll.fetch_secs = 0;
        let off = off.normalized();
        assert_eq!((off.poll.refresh_secs, off.poll.fetch_secs), (0, 0));
        assert_eq!(Prefs::default().normalized(), Prefs::default());
    }

    #[test]
    fn branch_names() {
        for ok in ["", "main", "feature/login", "release-1.0", "  trunk  "] {
            assert!(validate_branch_name(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "-x", "a b", "a..b", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b", "/a", "a/", "a//b",
            ".a", "a/.b", "a.", "a.lock", "a@{b", "@", "a\u{7}",
        ] {
            assert!(validate_branch_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Prefs::default().initial_branch(), None);
        let named = Prefs {
            default_branch: " main ".into(),
            ..Prefs::default()
        };
        assert_eq!(named.initial_branch(), Some("main"));
    }

    #[test]
    fn editor_path_must_be_an_absolute_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("editor.exe");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            validate_editor_path(file.to_str().unwrap()),
            Ok(file.clone())
        );
        assert!(validate_editor_path("").is_err());
        assert!(validate_editor_path("code").is_err());
        assert!(validate_editor_path("..\\code.cmd").is_err());
        assert!(validate_editor_path(dir.path().to_str().unwrap()).is_err());
        assert!(validate_editor_path(dir.path().join("missing").to_str().unwrap()).is_err());
    }

    #[test]
    fn pull_preference_maps_to_git_modes() {
        assert_eq!(PullPref::Config.mode(), PullMode::Config);
        assert_eq!(PullPref::Merge.mode(), PullMode::Merge);
        assert_eq!(PullPref::Rebase.mode(), PullMode::Rebase);
        assert_eq!(PullPref::FastForwardOnly.mode(), PullMode::FastForwardOnly);
    }
}
