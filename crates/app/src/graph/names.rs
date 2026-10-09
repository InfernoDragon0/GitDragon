//! Names a lane (a lane colour = one first-parent chain) for the hover tooltip: from the refs on
//! its tip, or, for a branch that was merged and deleted, from the merge commit's subject.

use git_core::graph::{EdgeKind, History};
use git_core::parse::log::RefLabel;

/// Branch name for lane `color` as seen from `row`: the nearest branch ref on that lane at or
/// above the row, or, where the lane was opened by a merge (a branch merged and deleted), the
/// name in the merge subject. Tags and a detached HEAD only when no branch is found.
pub fn lane_name(history: &History, color: u32, row: usize) -> Option<String> {
    let layout = history.layout();
    let mut fallback = None;
    for r in (0..=row.min(history.len().checked_sub(1)?)).rev() {
        let node = layout.node(r)?;
        if node.color == color {
            let refs = history.row(r)?.refs;
            if let Some(name) = branch_name(refs) {
                return Some(name);
            }
            fallback = fallback.or_else(|| other_name(refs));
            continue;
        }
        let merged: Vec<u32> = layout
            .edges(r)
            .iter()
            .filter(|e| e.kind == EdgeKind::Out && e.column != node.column)
            .map(|e| e.color)
            .collect();
        if let Some(k) = merged.iter().position(|&c| c == color) {
            // The merge that opened this lane names its other parents in order.
            return merged_name(history.row(r)?.subject, k).or(fallback);
        }
    }
    fallback
}

fn branch_name(refs: &[RefLabel]) -> Option<String> {
    let local = refs.iter().find_map(|r| match r {
        RefLabel::LocalBranch(n) => Some(n),
        _ => None,
    });
    let remote = || {
        refs.iter().find_map(|r| match r {
            RefLabel::RemoteBranch(n) => Some(n),
            _ => None,
        })
    };
    local.or_else(remote).cloned()
}

fn other_name(refs: &[RefLabel]) -> Option<String> {
    refs.iter()
        .find_map(|r| match r {
            RefLabel::Tag(n) | RefLabel::Other(n) => Some(n.clone()),
            _ => None,
        })
        .or_else(|| refs.contains(&RefLabel::Head).then(|| "HEAD".into()))
}

/// The `k`-th merged branch (0 = second parent) named by a merge subject:
/// - git: `Merge branch 'x'` / `… 'x' into y` / `… 'x' of <url>`, octopus
///   `Merge branches 'a', 'b' and 'c'`, `Merge remote-tracking branch 'origin/x'`
/// - GitHub: `Merge pull request #14 from owner/x`
/// - Azure DevOps: `Merged PR 22: Merge x into dev` (the branch), or `Merged PR 24: <title>`
///   (named `PR 24: <title>`)
/// - Bitbucket: `Merged in x (pull request #12)`
/// - Visual Studio / unquoted: `Merge x into y`
fn merged_name(subject: &str, k: usize) -> Option<String> {
    if let Some(rest) = subject.strip_prefix("Merge pull request ") {
        let (_, from) = rest.split_once(" from ")?;
        let name = from.split_whitespace().next()?;
        return (k == 0).then(|| name.to_string());
    }
    if let Some(rest) = subject.strip_prefix("Merged PR ") {
        let (number, title) = rest.split_once(':')?;
        let title = title.trim();
        if k != 0 || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(
            unquoted_merge(title)
                .unwrap_or_else(|| format!("PR {number}: {title}").trim_end().to_string()),
        );
    }
    if let Some(rest) = subject.strip_prefix("Merged in ") {
        let name = rest.split_whitespace().next()?;
        return (k == 0).then(|| name.to_string());
    }
    let rest = subject.strip_prefix("Merge ")?;
    let quoted = rest
        .strip_prefix("remote-tracking branches ")
        .or_else(|| rest.strip_prefix("remote-tracking branch "))
        .or_else(|| rest.strip_prefix("branches "))
        .or_else(|| rest.strip_prefix("branch "))
        .or_else(|| rest.strip_prefix("tags "))
        .or_else(|| rest.strip_prefix("tag "));
    match quoted {
        // Names are the quoted parts, before any `into` / `of`.
        Some(quoted) => quoted
            .split('\'')
            .skip(1)
            .step_by(2)
            .nth(k)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        None => unquoted_merge(subject).filter(|_| k == 0),
    }
}

/// `Merge x into y` with a single-word `x` (no quotes): `x`.
fn unquoted_merge(text: &str) -> Option<String> {
    let (from, _) = text.strip_prefix("Merge ")?.split_once(" into ")?;
    let plain = !from.is_empty() && !from.contains([' ', '\'', '"']);
    plain.then(|| from.to_string())
}

#[cfg(test)]
mod tests {
    use git_core::parse::log::{Commit, Signature};

    use super::*;

    fn commit(id: &str, parents: &[&str], subject: &str, refs: Vec<RefLabel>) -> Commit {
        let sig = Signature {
            name: "t".into(),
            email: "t@e".into(),
            time: 0,
            tz_offset_minutes: 0,
        };
        Commit {
            id: format!("{id:0>40}"),
            parents: parents.iter().map(|p| format!("{p:0>40}")).collect(),
            author: sig.clone(),
            committer: sig,
            subject: subject.into(),
            body: Default::default(),
            refs,
            head_target: None,
        }
    }

    #[test]
    fn merge_subjects() {
        assert_eq!(
            merged_name("Merge branch 'feature/login'", 0).as_deref(),
            Some("feature/login")
        );
        assert_eq!(
            merged_name("Merge branch 'main' into feature/graph", 0).as_deref(),
            Some("main")
        );
        assert_eq!(
            merged_name("Merge branch 'fix' of https://example.com/r.git", 0).as_deref(),
            Some("fix")
        );
        let octopus = "Merge branches 'docs/a', 'docs/b' and 'docs/c'";
        assert_eq!(merged_name(octopus, 1).as_deref(), Some("docs/b"));
        assert_eq!(merged_name(octopus, 3), None);
        assert_eq!(
            merged_name("Merge remote-tracking branch 'origin/main'", 0).as_deref(),
            Some("origin/main")
        );
        assert_eq!(
            merged_name("Merge pull request #14 from mushr/feature/graph", 0).as_deref(),
            Some("mushr/feature/graph")
        );
        // Azure DevOps.
        assert_eq!(
            merged_name("Merged PR 22: Merge zolice into dev", 0).as_deref(),
            Some("zolice")
        );
        assert_eq!(
            merged_name("Merged PR 24: [Feat][0.0.6] Combat, Systems", 0).as_deref(),
            Some("PR 24: [Feat][0.0.6] Combat, Systems")
        );
        assert_eq!(
            merged_name("Merged PR 4: zl", 0).as_deref(),
            Some("PR 4: zl")
        );
        assert_eq!(merged_name("Merged PR 4: zl", 1), None);
        assert_eq!(merged_name("Merged PR x: zl", 0), None);
        // Unquoted (Visual Studio) and Bitbucket.
        assert_eq!(
            merged_name("Merge dev into tree", 0).as_deref(),
            Some("dev")
        );
        assert_eq!(merged_name("Merge the fix into tree", 0), None);
        assert_eq!(
            merged_name("Merged in feature/x (pull request #12)", 0).as_deref(),
            Some("feature/x")
        );
        // GitLab quotes the target too.
        assert_eq!(
            merged_name("Merge branch 'feat' into 'main'", 0).as_deref(),
            Some("feat")
        );
        assert_eq!(merged_name("Add a feature", 0), None);
    }

    #[test]
    fn lanes_are_named_by_nearest_ref_or_merge_subject() {
        let mut h = History::new();
        h.extend([
            commit(
                "w",
                &["m"],
                "wip",
                vec![RefLabel::LocalBranch("feature/wip".into())],
            ),
            commit(
                "m",
                &["c", "f"],
                "Merge branch 'feature/login'",
                vec![RefLabel::LocalBranch("main".into())],
            ),
            commit("f", &["b"], "login", vec![]),
            commit("c", &["b"], "main work", vec![RefLabel::Tag("v1".into())]),
            commit("b", &[], "root", vec![]),
        ]);
        let color = |row: usize| h.layout().node(row).unwrap().color;
        // wip continues main's chain: same colour, named by the nearest ref above the row.
        assert_eq!(color(0), color(1));
        assert_eq!(lane_name(&h, color(0), 0).as_deref(), Some("feature/wip"));
        assert_eq!(lane_name(&h, color(3), 3).as_deref(), Some("main"));
        // The merged branch's lane is named by the merge subject.
        assert_eq!(lane_name(&h, color(2), 2).as_deref(), Some("feature/login"));
        assert_eq!(lane_name(&h, 999, 4), None);
    }

    #[test]
    fn tags_only_when_no_branch() {
        let mut h = History::new();
        h.extend([
            commit("t", &["b"], "tagged", vec![RefLabel::Tag("v2".into())]),
            commit("b", &[], "root", vec![]),
        ]);
        assert_eq!(lane_name(&h, 0, 1).as_deref(), Some("v2"));
        assert_eq!(
            branch_name(&[RefLabel::RemoteBranch("origin/x".into())]).as_deref(),
            Some("origin/x")
        );
        assert_eq!(other_name(&[RefLabel::Head]).as_deref(), Some("HEAD"));
    }
}
