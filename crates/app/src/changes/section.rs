//! Which part of the working copy a changed file is in: Conflicted / Staged / Unstaged /
//! Untracked. A file staged and then edited again is in both Staged and Unstaged.

use git_core::parse::status::{EntryKind, FileState, Status, StatusEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

impl Section {
    /// Whether `entry` belongs here.
    pub fn contains(self, entry: &StatusEntry) -> bool {
        let unmerged = entry.kind == EntryKind::Unmerged;
        match self {
            Section::Conflicted => unmerged,
            Section::Untracked => entry.kind == EntryKind::Untracked,
            Section::Staged => {
                !unmerged
                    && entry.kind != EntryKind::Untracked
                    && entry.index != FileState::Unmodified
            }
            Section::Unstaged => {
                !unmerged
                    && entry.kind != EntryKind::Untracked
                    && entry.worktree != FileState::Unmodified
            }
        }
    }

    /// The entries of `status` in this section, in status order.
    pub fn entries(self, status: &Status) -> Vec<StatusEntry> {
        status
            .entries
            .iter()
            .filter(|e| self.contains(e))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind, x: FileState, y: FileState, path: &str) -> StatusEntry {
        StatusEntry {
            kind,
            index: x,
            worktree: y,
            path: path.into(),
            orig_path: None,
            submodule: false,
        }
    }

    #[test]
    fn a_partly_staged_file_is_in_both_sections() {
        use FileState::*;
        let status = Status {
            entries: vec![
                entry(EntryKind::Ordinary, Modified, Modified, "both.rs"),
                entry(EntryKind::Ordinary, Added, Unmodified, "new.rs"),
                entry(EntryKind::Untracked, Unmodified, Unmodified, "notes.md"),
                entry(EntryKind::Unmerged, Unmerged, Unmerged, "c.txt"),
            ],
            ..Default::default()
        };
        let paths = |section: Section| {
            section
                .entries(&status)
                .into_iter()
                .map(|e| e.path.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(Section::Staged), ["both.rs", "new.rs"]);
        assert_eq!(paths(Section::Unstaged), ["both.rs"]);
        assert_eq!(paths(Section::Untracked), ["notes.md"]);
        assert_eq!(paths(Section::Conflicted), ["c.txt"]);
    }
}
