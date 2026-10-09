//! What GitDragon remembers between runs: recent repos, open tabs, theme, dock layout.
//! Stored as JSON in the platform config dir (Windows:
//! `%APPDATA%\GitDragon\GitDragon\config\state.json`);
//! `GITDRAGON_STATE_DIR` overrides the folder.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "state.json";
const STATE_DIR_ENV: &str = "GITDRAGON_STATE_DIR";
pub const RECENT_MAX: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentRepo {
    pub path: PathBuf,
    pub name: String,
    /// Unix seconds.
    pub last_opened: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DockLayout {
    pub left_open: bool,
    pub right_open: bool,
    pub left_width: f32,
    pub right_width: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedState {
    /// Most recent first.
    pub recent: Vec<RecentRepo>,
    pub open_tabs: Vec<PathBuf>,
    pub active_tab: Option<usize>,
    /// `Appearance::parse` text, e.g. `onyx-dark`.
    pub appearance: Option<String>,
    pub dock: Option<DockLayout>,
    /// Folder the clone dialog suggested last.
    pub clone_parent: Option<PathBuf>,
}

impl SavedState {
    /// Missing file means defaults; a corrupt file is kept as `state.json.bad` and replaced.
    pub fn load() -> Self {
        let Some(path) = state_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "state file unreadable; starting fresh");
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        if let Err(err) = self.save_to(&path) {
            tracing::warn!(path = %path.display(), %err, "could not save state");
        }
    }

    /// Writes a temp file then renames it, so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Moves (or adds) `path` to the top of the recent list.
    pub fn touch_recent(&mut self, path: &Path) {
        self.recent.retain(|r| r.path != path);
        self.recent.insert(
            0,
            RecentRepo {
                path: path.to_path_buf(),
                name: repo_name(path),
                last_opened: now(),
            },
        );
        self.recent.truncate(RECENT_MAX);
    }

    pub fn forget_recent(&mut self, path: &Path) {
        self.recent.retain(|r| r.path != path);
    }
}

/// Display name for a repo: its folder name.
pub fn repo_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn state_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Some(PathBuf::from(dir).join(FILE_NAME));
    }
    use etcetera::AppStrategy as _;
    let strategy = etcetera::choose_app_strategy(etcetera::AppStrategyArgs {
        top_level_domain: "dev".into(),
        author: "GitDragon".into(),
        app_name: "GitDragon".into(),
    })
    .ok()?;
    Some(strategy.config_dir().join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_junk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(SavedState::load_from(&path), SavedState::default());

        let mut state = SavedState::default();
        state.touch_recent(Path::new("/r/one"));
        state.appearance = Some("onyx-light".into());
        state.save_to(&path).unwrap();
        assert_eq!(SavedState::load_from(&path), state);

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(SavedState::load_from(&path), SavedState::default());
        assert!(path.with_extension("json.bad").exists());

        // Unknown and missing fields are fine (older / newer versions).
        std::fs::write(&path, r#"{"future_field": 1, "open_tabs": ["/x"]}"#).unwrap();
        assert_eq!(
            SavedState::load_from(&path).open_tabs,
            vec![PathBuf::from("/x")]
        );
    }

    #[test]
    fn recent_list_is_mru_and_capped() {
        let mut state = SavedState::default();
        for i in 0..RECENT_MAX + 5 {
            state.touch_recent(Path::new(&format!("/r/{i}")));
        }
        state.touch_recent(Path::new("/r/3"));
        assert_eq!(state.recent.len(), RECENT_MAX);
        assert_eq!(state.recent[0].path, PathBuf::from("/r/3"));
        assert_eq!(state.recent[0].name, "3");
        assert_eq!(state.recent.iter().filter(|r| r.name == "3").count(), 1);
        state.forget_recent(Path::new("/r/3"));
        assert!(state.recent.iter().all(|r| r.name != "3"));
    }
}
