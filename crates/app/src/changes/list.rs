//! The file list on the left of the Changes view: status entries grouped into Conflicted /
//! Staged / Unstaged / Untracked, filtered, shown flat or as a folder tree. One flat row list,
//! so the view can virtualise it with a uniform list.

use bstr::{BString, ByteSlice};
use git_core::parse::status::{EntryKind, FileState, Status, StatusEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Section {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Conflicted,
        Section::Staged,
        Section::Unstaged,
        Section::Untracked,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Conflicted => "Conflicted",
            Section::Staged => "Staged",
            Section::Unstaged => "Unstaged",
            Section::Untracked => "Untracked",
        }
    }

    /// Whether `entry` belongs here. A file staged and then edited again is in both Staged
    /// and Unstaged.
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

    /// The state letter this section shows for `entry`.
    pub fn state(self, entry: &StatusEntry) -> FileState {
        match self {
            Section::Staged => entry.index,
            Section::Unstaged => entry.worktree,
            Section::Conflicted => FileState::Unmerged,
            Section::Untracked => FileState::Added,
        }
    }
}

/// Letter shown in the status badge.
pub fn state_letter(state: FileState) -> &'static str {
    match state {
        FileState::Modified => "M",
        FileState::TypeChanged => "T",
        FileState::Added => "A",
        FileState::Deleted => "D",
        FileState::Renamed => "R",
        FileState::Copied => "C",
        FileState::Unmerged => "U",
        FileState::Unmodified => "·",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
    Header {
        section: Section,
        count: usize,
    },
    /// Tree mode: a folder (path without trailing `/`).
    Dir {
        section: Section,
        path: BString,
        depth: usize,
    },
    File {
        section: Section,
        /// Index into `Status::entries`.
        entry: usize,
        depth: usize,
    },
}

/// Rows for `status`. `filter` matches case-insensitively anywhere in the path.
pub fn build(status: &Status, tree: bool, filter: &str) -> Vec<ListRow> {
    let filter = filter.trim().to_lowercase();
    let mut rows = Vec::new();
    for section in Section::ALL {
        let mut entries: Vec<usize> = status
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| section.contains(e))
            .filter(|(_, e)| {
                filter.is_empty() || e.path.to_str_lossy().to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect();
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|&a, &b| status.entries[a].path.cmp(&status.entries[b].path));
        rows.push(ListRow::Header {
            section,
            count: entries.len(),
        });
        if !tree {
            rows.extend(entries.into_iter().map(|entry| ListRow::File {
                section,
                entry,
                depth: 0,
            }));
            continue;
        }
        // Sorted paths: emit each folder the first time a file inside it shows up.
        let mut open: Vec<&[u8]> = Vec::new();
        for entry in entries {
            let path = status.entries[entry].path.as_slice();
            let dirs: Vec<&[u8]> = path.find_iter(b"/").map(|slash| &path[..slash]).collect();
            let shared = open.iter().zip(&dirs).take_while(|(a, b)| a == b).count();
            open.truncate(shared);
            for dir in &dirs[shared..] {
                rows.push(ListRow::Dir {
                    section,
                    path: BString::from(*dir),
                    depth: open.len(),
                });
                open.push(dir);
            }
            rows.push(ListRow::File {
                section,
                entry,
                depth: dirs.len(),
            });
        }
    }
    rows
}

/// Last path component (tree mode shows names under their folder).
pub fn file_name(path: &[u8]) -> &[u8] {
    path.rfind_byte(b'/')
        .map_or(path, |slash| &path[slash + 1..])
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

    fn status() -> Status {
        use EntryKind::*;
        use FileState::*;
        Status {
            entries: vec![
                entry(Ordinary, Modified, Modified, "src/b.rs"),
                entry(Ordinary, Unmodified, Deleted, "src/a/x.rs"),
                entry(Untracked, Unmodified, Unmodified, "notes.md"),
                entry(
                    EntryKind::Unmerged,
                    FileState::Unmerged,
                    FileState::Unmerged,
                    "c.txt",
                ),
                entry(Ordinary, Added, Unmodified, "src/a/y.rs"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn flat_groups_and_sorts() {
        let rows = build(&status(), false, "");
        let headers: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                ListRow::Header { section, count } => Some((*section, *count)),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                (Section::Conflicted, 1),
                (Section::Staged, 2),
                (Section::Unstaged, 2),
                (Section::Untracked, 1),
            ]
        );
        // Staged: src/a/y.rs before src/b.rs.
        assert_eq!(
            rows[3],
            ListRow::File {
                section: Section::Staged,
                entry: 4,
                depth: 0
            }
        );
    }

    #[test]
    fn tree_emits_each_folder_once() {
        let rows = build(&status(), true, "");
        let unstaged: Vec<_> = rows
            .iter()
            .filter(|r| match r {
                ListRow::Dir { section, .. } | ListRow::File { section, .. } => {
                    *section == Section::Unstaged
                }
                _ => false,
            })
            .cloned()
            .collect();
        assert_eq!(
            unstaged,
            vec![
                ListRow::Dir {
                    section: Section::Unstaged,
                    path: "src".into(),
                    depth: 0
                },
                ListRow::Dir {
                    section: Section::Unstaged,
                    path: "src/a".into(),
                    depth: 1
                },
                ListRow::File {
                    section: Section::Unstaged,
                    entry: 1,
                    depth: 2
                },
                ListRow::File {
                    section: Section::Unstaged,
                    entry: 0,
                    depth: 1
                },
            ]
        );
    }

    #[test]
    fn filter_hides_empty_sections() {
        let rows = build(&status(), false, "NOTES");
        assert_eq!(rows.len(), 2);
        assert_eq!(file_name(b"a/b/c.rs"), b"c.rs");
        assert_eq!(file_name(b"c.rs"), b"c.rs");
    }
}
