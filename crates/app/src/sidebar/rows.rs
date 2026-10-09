//! The sidebar tree flattened into rows for a virtual list: section headers, local branches,
//! remotes with their branches, tags, stashes and submodules, filtered by the search box.

use std::collections::HashSet;

use git_core::parse::refs::{RefEntry, RefKind, Remote, Stash};
use git_core::{Submodule, SubmoduleState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Local,
    Remotes,
    Tags,
    Stashes,
    Submodules,
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Section::Local => "LOCAL",
            Section::Remotes => "REMOTES",
            Section::Tags => "TAGS",
            Section::Stashes => "STASHES",
            Section::Submodules => "SUBMODULES",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header {
        section: Section,
        count: usize,
        collapsed: bool,
    },
    Local {
        name: String,
        oid: String,
        current: bool,
        /// Short upstream, e.g. `origin/main`.
        upstream: Option<String>,
        ahead: u32,
        behind: u32,
        gone: bool,
    },
    Remote {
        name: String,
        url: Option<String>,
        count: usize,
        collapsed: bool,
    },
    RemoteBranch {
        remote: String,
        /// Without the remote, e.g. `feature/x`.
        branch: String,
        oid: String,
    },
    Tag {
        name: String,
        /// The commit (peeled for annotated tags).
        oid: String,
    },
    Stash {
        index: u32,
        message: String,
        oid: String,
    },
    Submodule {
        path: String,
        state: SubmoduleState,
    },
    /// Placeholder line inside an empty section.
    Empty(Section),
}

/// Everything the tree is built from (copied out of the repo tab).
pub struct Data<'a> {
    pub refs: &'a [RefEntry],
    pub remotes: &'a [Remote],
    pub stashes: &'a [Stash],
    pub submodules: &'a [Submodule],
    /// Checked-out branch, `None` when detached.
    pub head: Option<&'a str>,
}

pub struct View<'a> {
    /// Lowercased search text; empty shows everything.
    pub filter: &'a str,
    pub collapsed: &'a HashSet<Section>,
    pub collapsed_remotes: &'a HashSet<String>,
}

pub fn build(data: &Data<'_>, view: &View<'_>) -> Vec<Row> {
    let hit = |text: &str| view.filter.is_empty() || text.to_lowercase().contains(view.filter);
    let mut rows = Vec::new();
    let section = |rows: &mut Vec<Row>, section: Section, items: Vec<Row>, count: usize| {
        let collapsed = view.collapsed.contains(&section);
        // While searching, sections without matches disappear.
        if !view.filter.is_empty() && count == 0 {
            return;
        }
        rows.push(Row::Header {
            section,
            count,
            collapsed,
        });
        if collapsed {
            return;
        }
        if items.is_empty() {
            rows.push(Row::Empty(section));
        }
        rows.extend(items);
    };

    let locals: Vec<Row> = data
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch && hit(&r.short_name))
        .map(|r| {
            let up = r.upstream.as_ref();
            Row::Local {
                name: r.short_name.clone(),
                oid: r.oid.clone(),
                current: r.is_head || data.head == Some(r.short_name.as_str()),
                upstream: up.map(|u| {
                    u.name
                        .strip_prefix("refs/remotes/")
                        .unwrap_or(&u.name)
                        .to_string()
                }),
                ahead: up.map_or(0, |u| u.ahead),
                behind: up.map_or(0, |u| u.behind),
                gone: up.is_some_and(|u| u.gone),
            }
        })
        .collect();
    let n = locals.len();
    section(&mut rows, Section::Local, locals, n);

    // Remotes: configured ones in order, plus any remote-tracking refs of unknown remotes.
    let mut names: Vec<String> = data.remotes.iter().map(|r| r.name.clone()).collect();
    for r in data.refs.iter().filter(|r| r.kind == RefKind::RemoteBranch) {
        if let Some((remote, _)) = remote_branch(r, data.remotes)
            && !names.contains(&remote)
        {
            names.push(remote);
        }
    }
    let mut remote_rows = Vec::new();
    let mut remote_count = 0;
    for name in names {
        let branches: Vec<Row> = data
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::RemoteBranch)
            .filter_map(|r| {
                let (remote, branch) = remote_branch(r, data.remotes)?;
                (remote == name && (hit(&branch) || hit(&name))).then(|| Row::RemoteBranch {
                    remote,
                    branch,
                    oid: r.oid.clone(),
                })
            })
            .collect();
        if !view.filter.is_empty() && branches.is_empty() && !hit(&name) {
            continue;
        }
        remote_count += 1;
        let collapsed = view.collapsed_remotes.contains(&name);
        remote_rows.push(Row::Remote {
            url: data
                .remotes
                .iter()
                .find(|r| r.name == name)
                .and_then(|r| r.fetch_url.clone()),
            count: branches.len(),
            collapsed,
            name,
        });
        if !collapsed {
            remote_rows.extend(branches);
        }
    }
    section(&mut rows, Section::Remotes, remote_rows, remote_count);

    let tags: Vec<Row> = data
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::Tag && hit(&r.short_name))
        .map(|r| Row::Tag {
            name: r.short_name.clone(),
            oid: r.peeled_oid.clone().unwrap_or_else(|| r.oid.clone()),
        })
        .collect();
    let n = tags.len();
    section(&mut rows, Section::Tags, tags, n);

    let stashes: Vec<Row> = data
        .stashes
        .iter()
        .map(|s| (s, String::from_utf8_lossy(&s.message).into_owned()))
        .filter(|(_, message)| hit(message))
        .map(|(s, message)| Row::Stash {
            index: s.index,
            message,
            oid: s.oid.clone(),
        })
        .collect();
    let n = stashes.len();
    section(&mut rows, Section::Stashes, stashes, n);

    // Only repos that have submodules get the section.
    if !data.submodules.is_empty() {
        let subs: Vec<Row> = data
            .submodules
            .iter()
            .filter(|s| hit(&s.path))
            .map(|s| Row::Submodule {
                path: s.path.clone(),
                state: s.state,
            })
            .collect();
        let n = subs.len();
        section(&mut rows, Section::Submodules, subs, n);
    }
    rows
}

/// `(remote, branch)` of a remote-tracking ref; `None` for `<remote>/HEAD`. Matches the
/// longest configured remote name first (remote names may contain `/`-free prefixes of others).
fn remote_branch(r: &RefEntry, remotes: &[Remote]) -> Option<(String, String)> {
    let rest = r.full_name.strip_prefix("refs/remotes/")?;
    let mut known: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
    known.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let (remote, branch) = known
        .iter()
        .find_map(|n| {
            rest.strip_prefix(n)
                .and_then(|b| b.strip_prefix('/'))
                .map(|b| (n.to_string(), b.to_string()))
        })
        .or_else(|| {
            rest.split_once('/')
                .map(|(r, b)| (r.to_string(), b.to_string()))
        })?;
    (branch != "HEAD").then_some((remote, branch))
}

#[cfg(test)]
mod tests {
    use bstr::BString;
    use git_core::parse::refs::Upstream;

    use super::*;

    fn entry(full: &str, kind: RefKind) -> RefEntry {
        let short = full
            .strip_prefix("refs/heads/")
            .or_else(|| full.strip_prefix("refs/remotes/"))
            .or_else(|| full.strip_prefix("refs/tags/"))
            .unwrap_or(full);
        RefEntry {
            full_name: full.into(),
            short_name: short.into(),
            kind,
            oid: format!("{:0>40}", full.len()),
            peeled_oid: None,
            is_head: false,
            upstream: None,
            date: 0,
            subject: BString::default(),
        }
    }

    fn data() -> (Vec<RefEntry>, Vec<Remote>, Vec<Stash>) {
        let mut main = entry("refs/heads/main", RefKind::LocalBranch);
        main.is_head = true;
        main.upstream = Some(Upstream {
            name: "refs/remotes/origin/main".into(),
            ahead: 2,
            behind: 1,
            gone: false,
        });
        let mut tag = entry("refs/tags/v1", RefKind::Tag);
        tag.peeled_oid = Some("c".repeat(40));
        let refs = vec![
            main,
            entry("refs/heads/feature/x", RefKind::LocalBranch),
            entry("refs/remotes/origin/HEAD", RefKind::RemoteBranch),
            entry("refs/remotes/origin/main", RefKind::RemoteBranch),
            entry("refs/remotes/origin/feature/x", RefKind::RemoteBranch),
            entry("refs/remotes/old/main", RefKind::RemoteBranch),
            tag,
        ];
        let remotes = vec![Remote {
            name: "origin".into(),
            fetch_url: Some("https://example.com/r.git".into()),
            push_url: None,
        }];
        let stashes = vec![Stash {
            index: 0,
            oid: "d".repeat(40),
            time: 0,
            message: "WIP on main: abc fix".into(),
        }];
        (refs, remotes, stashes)
    }

    fn rows(filter: &str, collapsed: &[Section], collapsed_remotes: &[&str]) -> Vec<Row> {
        let (refs, remotes, stashes) = data();
        let collapsed: HashSet<Section> = collapsed.iter().copied().collect();
        let collapsed_remotes: HashSet<String> =
            collapsed_remotes.iter().map(|s| s.to_string()).collect();
        build(
            &Data {
                refs: &refs,
                remotes: &remotes,
                stashes: &stashes,
                submodules: &[],
                head: Some("main"),
            },
            &View {
                filter,
                collapsed: &collapsed,
                collapsed_remotes: &collapsed_remotes,
            },
        )
    }

    #[test]
    fn full_tree() {
        let rows = rows("", &[], &[]);
        let main = rows
            .iter()
            .find(|r| matches!(r, Row::Local { name, .. } if name == "main"))
            .unwrap();
        assert_eq!(
            main,
            &Row::Local {
                name: "main".into(),
                oid: entry("refs/heads/main", RefKind::LocalBranch).oid,
                current: true,
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                gone: false,
            }
        );
        // origin/HEAD is hidden; an unknown remote (`old`) still gets its group.
        let remotes: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Remote { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(remotes, ["origin", "old"]);
        let origin_branches: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::RemoteBranch { remote, branch, .. } if remote == "origin" => {
                    Some(branch.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(origin_branches, ["main", "feature/x"]);
        assert!(rows.contains(&Row::Tag {
            name: "v1".into(),
            oid: "c".repeat(40)
        }));
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::Stash { index: 0, .. }))
        );
        // No submodules: no section.
        assert!(!rows.iter().any(|r| matches!(
            r,
            Row::Header {
                section: Section::Submodules,
                ..
            }
        )));
    }

    #[test]
    fn collapsing_and_filtering() {
        let rows_collapsed = rows("", &[Section::Local], &["origin"]);
        assert!(
            !rows_collapsed
                .iter()
                .any(|r| matches!(r, Row::Local { .. }))
        );
        assert!(rows_collapsed.contains(&Row::Header {
            section: Section::Local,
            count: 2,
            collapsed: true
        }));
        assert!(
            !rows_collapsed
                .iter()
                .any(|r| matches!(r, Row::RemoteBranch { remote, .. } if remote == "origin"))
        );

        let filtered = rows("feature", &[], &[]);
        assert!(
            filtered
                .iter()
                .any(|r| matches!(r, Row::Local { name, .. } if name == "feature/x"))
        );
        assert!(
            !filtered
                .iter()
                .any(|r| matches!(r, Row::Local { name, .. } if name == "main"))
        );
        // Sections without matches disappear while searching.
        assert!(!filtered.iter().any(|r| matches!(
            r,
            Row::Header {
                section: Section::Tags,
                ..
            }
        )));
        assert!(
            !filtered
                .iter()
                .any(|r| matches!(r, Row::Remote { name, .. } if name == "old"))
        );
    }

    #[test]
    fn empty_sections_show_a_placeholder() {
        let rows = build(
            &Data {
                refs: &[],
                remotes: &[],
                stashes: &[],
                submodules: &[],
                head: None,
            },
            &View {
                filter: "",
                collapsed: &HashSet::new(),
                collapsed_remotes: &HashSet::new(),
            },
        );
        assert!(rows.contains(&Row::Empty(Section::Local)));
        assert!(rows.contains(&Row::Empty(Section::Stashes)));
    }
}
