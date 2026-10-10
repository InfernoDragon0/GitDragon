//! Which ref badges a graph row shows: HEAD folds into the branch it points at, a remote branch
//! folds into the local one of the same name, then the most important few are kept and the
//! rest counted for the "+N" chip (all of them are listed in the row's hover).

use git_core::parse::log::RefLabel;

/// Badges drawn in a row before the rest collapse into "+N".
pub const MAX_BADGES: usize = 2;

/// One badge: a ref, with HEAD and same-named remote branches folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub label: RefLabel,
    /// HEAD points here (the current branch, or a detached `HEAD` badge).
    pub head: bool,
    /// Remote branches with this local branch's name, e.g. `origin/main` on `main`.
    pub remotes: Vec<String>,
}

impl Badge {
    fn new(label: RefLabel) -> Self {
        Self {
            label,
            head: false,
            remotes: Vec::new(),
        }
    }

    /// Lower sorts first: current branch / HEAD, local branches, tags, remotes, the rest.
    fn rank(&self) -> u8 {
        match (&self.label, self.head) {
            (_, true) => 0,
            (RefLabel::LocalBranch(_), _) => 1,
            (RefLabel::Tag(_), _) => 2,
            (RefLabel::RemoteBranch(_), _) => 3,
            _ => 4,
        }
    }
}

/// A row's refs as badges, most important first (git's order kept within a kind).
/// `refs/stash` is left out: the stash row's own `stash@{n}` pill already says it.
pub fn badges(refs: &[RefLabel]) -> Vec<Badge> {
    let mut out: Vec<Badge> = Vec::new();
    let mut iter = refs.iter().peekable();
    while let Some(label) = iter.next() {
        match label {
            RefLabel::Other(name) if name == "refs/stash" => {}
            // `HEAD -> x` comes as HEAD then x: one badge for the current branch.
            RefLabel::Head => match iter.next_if(|l| matches!(l, RefLabel::LocalBranch(_))) {
                Some(branch) => out.push(Badge {
                    head: true,
                    ..Badge::new(branch.clone())
                }),
                None => out.push(Badge {
                    head: true,
                    ..Badge::new(RefLabel::Head)
                }),
            },
            other => out.push(Badge::new(other.clone())),
        }
    }
    // Fold `remote/name` into the local `name` badge.
    let remotes: Vec<String> = out
        .iter()
        .filter_map(|b| match &b.label {
            RefLabel::RemoteBranch(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    for remote in remotes {
        let Some((_, short)) = remote.split_once('/') else {
            continue;
        };
        let local = out
            .iter_mut()
            .find(|b| matches!(&b.label, RefLabel::LocalBranch(n) if n == short));
        if let Some(local) = local {
            local.remotes.push(remote.clone());
            out.retain(|b| !matches!(&b.label, RefLabel::RemoteBranch(n) if *n == remote));
        }
    }
    out.sort_by_key(Badge::rank);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(n: &str) -> RefLabel {
        RefLabel::LocalBranch(n.into())
    }
    fn remote(n: &str) -> RefLabel {
        RefLabel::RemoteBranch(n.into())
    }
    fn tag(n: &str) -> RefLabel {
        RefLabel::Tag(n.into())
    }

    #[test]
    fn ranks_head_locals_tags_then_remotes() {
        let refs = [
            tag("v1.0"),
            remote("origin/feature"),
            local("dev"),
            RefLabel::Head,
            local("main"),
            remote("origin/main"),
            local("topic"),
        ];
        let got = badges(&refs);
        let labels: Vec<&RefLabel> = got.iter().map(|b| &b.label).collect();
        assert_eq!(
            labels,
            [
                &local("main"),
                &local("dev"),
                &local("topic"),
                &tag("v1.0"),
                &remote("origin/feature"),
            ]
        );
        assert!(got[0].head);
        assert_eq!(got[0].remotes, ["origin/main"]);
        assert!(got[1..].iter().all(|b| !b.head && b.remotes.is_empty()));
        // Two shown, the rest go to "+N".
        assert_eq!(got.len().saturating_sub(MAX_BADGES), 3);
    }

    #[test]
    fn detached_head_and_stash() {
        let refs = [
            remote("upstream/x"),
            RefLabel::Head,
            tag("v2"),
            RefLabel::Other("refs/stash".into()),
        ];
        let got = badges(&refs);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].label, RefLabel::Head);
        assert!(got[0].head);
        assert_eq!(got[1].label, tag("v2"));
        // No local `x`: the remote stays its own badge.
        assert_eq!(got[2].label, remote("upstream/x"));
    }

    #[test]
    fn several_remotes_fold_into_one_local() {
        let refs = [local("main"), remote("origin/main"), remote("fork/main")];
        let got = badges(&refs);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remotes, ["origin/main", "fork/main"]);
    }
}
