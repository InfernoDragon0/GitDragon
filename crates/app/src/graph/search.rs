//! Graph search: which rows match the search box. Matches are highlighted and the rest dimmed
//! (rows are never hidden: hiding them would break the lanes).
//!
//! - `text`: subject or author name / email contains it (case-insensitive), or a commit id
//!   starts with it.
//! - `author:ann`: author name or email contains `ann`.
//! - `hash:a81f`: commit id starts with `a81f`.
//! - `path:src/x.rs`: commits touching that path (asked from git, see [`Query::path`]).

use std::collections::HashSet;

use git_core::graph::CommitRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Text(String),
    Author(String),
    Hash(String),
    Path(String),
}

impl Query {
    /// `None` for an empty search box.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let field = |prefix: &str| {
            input
                .get(..prefix.len())
                .filter(|p| p.eq_ignore_ascii_case(prefix))
                .map(|_| input[prefix.len()..].trim().to_string())
                .filter(|rest| !rest.is_empty())
        };
        Some(if let Some(rest) = field("author:") {
            Query::Author(rest.to_lowercase())
        } else if let Some(rest) = field("hash:") {
            Query::Hash(rest.to_lowercase())
        } else if let Some(rest) = field("path:") {
            Query::Path(rest)
        } else {
            Query::Text(input.to_lowercase())
        })
    }

    /// The path a `path:` query needs git to resolve.
    pub fn path(&self) -> Option<&str> {
        match self {
            Query::Path(path) => Some(path),
            _ => None,
        }
    }

    /// Does `row` match? `path_ids` are the commits touching the path of a `path:` query
    /// (`None` while git is still answering: nothing matches yet).
    pub fn matches(&self, row: &CommitRow<'_>, path_ids: Option<&HashSet<String>>) -> bool {
        let author = || {
            [&row.author.name, &row.author.email]
                .iter()
                .any(|s| contains_lower(s, self.needle()))
        };
        match self {
            Query::Text(t) => {
                row.id.starts_with(t.as_str()) || contains_lower(row.subject, t) || author()
            }
            Query::Author(_) => author(),
            Query::Hash(h) => row.id.starts_with(h.as_str()),
            Query::Path(_) => path_ids.is_some_and(|ids| ids.contains(row.id)),
        }
    }

    fn needle(&self) -> &str {
        match self {
            Query::Text(s) | Query::Author(s) | Query::Hash(s) | Query::Path(s) => s,
        }
    }
}

/// `haystack` contains `needle_lower` (already lowercased), ignoring case.
fn contains_lower(haystack: &str, needle_lower: &str) -> bool {
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let needle = needle_lower.as_bytes();
        return needle.is_empty()
            || haystack
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle));
    }
    haystack.to_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use git_core::graph::{Author, Node};

    use super::*;

    fn row<'a>(id: &'a str, subject: &'a str, author: &'a Author) -> CommitRow<'a> {
        CommitRow {
            id,
            subject,
            author,
            time: 0,
            tz_offset_minutes: 0,
            refs: &[],
            co_authors: &[],
            node: Node {
                column: 0,
                color: 0,
            },
            edges: &[],
        }
    }

    #[test]
    fn parse_prefixes() {
        assert_eq!(Query::parse("  "), None);
        assert_eq!(Query::parse("Fix Bug"), Some(Query::Text("fix bug".into())));
        assert_eq!(
            Query::parse("Author: Ada"),
            Some(Query::Author("ada".into()))
        );
        assert_eq!(Query::parse("hash:A81F"), Some(Query::Hash("a81f".into())));
        assert_eq!(
            Query::parse("path:src/Main.rs"),
            Some(Query::Path("src/Main.rs".into()))
        );
        // A bare prefix is just text.
        assert_eq!(Query::parse("author:"), Some(Query::Text("author:".into())));
    }

    #[test]
    fn matching() {
        let ada = Author {
            name: "Ada Lovelace".into(),
            email: "ada@example.com".into(),
        };
        let r = row("a81f2c3d", "graph: Incremental lanes", &ada);
        let m = |q: &str| Query::parse(q).unwrap().matches(&r, None);
        assert!(m("incremental"));
        assert!(m("LOVELACE"));
        assert!(m("a81f"));
        assert!(!m("81f"));
        assert!(m("author:example.com"));
        assert!(!m("author:lanes"));
        assert!(m("hash:A81F2"));
        assert!(!m("path:src"));
        let ids: HashSet<String> = ["a81f2c3d".to_string()].into();
        assert!(Query::parse("path:src").unwrap().matches(&r, Some(&ids)));
    }

    #[test]
    fn unicode_case_folding() {
        let who = Author {
            name: "Željko".into(),
            email: "z@e".into(),
        };
        let r = row("00", "Größe anpassen", &who);
        assert!(Query::parse("GRÖSSE").is_some());
        assert!(Query::parse("größe").unwrap().matches(&r, None));
        assert!(Query::parse("željko").unwrap().matches(&r, None));
    }
}
