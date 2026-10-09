//! "Undo" for the operations a reflog can take back (reset, merge, rebase, pull, cherry-pick,
//! revert, branch delete). The tab notes the state before such an operation and, if it
//! succeeded and changed something, keeps an [`UndoRecord`] for [`WINDOW`]; the toast and the
//! operation strip offer the button while it is fresh. The git side (and its safety checks) is
//! `git_core::UndoAction`.

use std::time::{Duration, Instant};

use git_core::{HeadState, UndoAction};

/// How long the Undo button stays.
pub const WINDOW: Duration = Duration::from_secs(30);

/// What to note before an operation runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPlan {
    /// The operation moves HEAD (reset, merge, rebase, pull, cherry-pick, revert).
    Head,
    /// The operation deletes this local branch.
    Branch(String),
}

/// Moving HEAD is undoable when a commit was and still is checked out on the same branch (or
/// detached both times) and HEAD ended somewhere else. Switching branches is not an undo case.
pub fn head_undo(before: &HeadState, after: &HeadState) -> Option<UndoAction> {
    let (from, to) = (after.oid.clone()?, before.oid.clone()?);
    (from != to && before.branch == after.branch).then(|| UndoAction::MoveHeadBack {
        branch: after.branch.clone(),
        from,
        to,
    })
}

/// A deleted branch is restorable when it had a tip and is gone now.
pub fn branch_undo(name: &str, tip: Option<String>, exists_now: bool) -> Option<UndoAction> {
    let tip = tip?;
    (!exists_now).then(|| UndoAction::RestoreBranch {
        name: name.to_string(),
        tip,
    })
}

/// One undo on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoRecord {
    /// Distinguishes this record from earlier ones (toasts are matched by it).
    pub id: u64,
    /// The operation it reverses, e.g. `Merging topic`.
    pub label: String,
    pub action: UndoAction,
    made: Instant,
}

impl UndoRecord {
    pub fn new(id: u64, label: impl Into<String>, action: UndoAction, made: Instant) -> Self {
        Self {
            id,
            label: label.into(),
            action,
            made,
        }
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.made) < WINDOW
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        WINDOW.saturating_sub(now.saturating_duration_since(self.made))
    }

    /// What the button does, for its tooltip.
    pub fn describe(&self) -> String {
        match &self.action {
            UndoAction::MoveHeadBack { to, .. } => {
                format!(
                    "Move the branch back to {} (git reset --keep)",
                    &to[..to.len().min(7)]
                )
            }
            UndoAction::RestoreBranch { name, tip } => {
                format!("Recreate {name} at {}", &tip[..tip.len().min(7)])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn head(c: char, branch: Option<&str>) -> HeadState {
        HeadState {
            oid: Some(oid(c)),
            branch: branch.map(str::to_string),
        }
    }

    #[test]
    fn moved_head_on_the_same_branch_is_undoable() {
        let undo = head_undo(&head('a', Some("main")), &head('b', Some("main"))).unwrap();
        assert_eq!(
            undo,
            UndoAction::MoveHeadBack {
                branch: Some("main".into()),
                from: oid('b'),
                to: oid('a'),
            }
        );
        // Detached both times counts too.
        assert!(head_undo(&head('a', None), &head('b', None)).is_some());
    }

    #[test]
    fn nothing_to_undo_when_head_did_not_move_or_branch_changed() {
        assert_eq!(
            head_undo(&head('a', Some("main")), &head('a', Some("main"))),
            None
        );
        assert_eq!(
            head_undo(&head('a', Some("main")), &head('b', Some("dev"))),
            None
        );
        assert_eq!(head_undo(&head('a', Some("main")), &head('b', None)), None);
        let unborn = HeadState {
            oid: None,
            branch: Some("main".into()),
        };
        assert_eq!(head_undo(&unborn, &head('b', Some("main"))), None);
        assert_eq!(head_undo(&head('a', Some("main")), &unborn), None);
    }

    #[test]
    fn deleted_branches_come_back_only_when_gone() {
        assert_eq!(
            branch_undo("topic", Some(oid('c')), false),
            Some(UndoAction::RestoreBranch {
                name: "topic".into(),
                tip: oid('c')
            })
        );
        assert_eq!(branch_undo("topic", Some(oid('c')), true), None);
        assert_eq!(branch_undo("topic", None, false), None);
    }

    #[test]
    fn records_expire_after_the_window() {
        let t0 = Instant::now();
        let record = UndoRecord::new(
            7,
            "Merging topic",
            UndoAction::RestoreBranch {
                name: "x".into(),
                tip: oid('d'),
            },
            t0,
        );
        assert!(record.is_fresh(t0));
        assert!(record.is_fresh(t0 + WINDOW - Duration::from_secs(1)));
        assert!(!record.is_fresh(t0 + WINDOW));
        assert_eq!(
            record.remaining(t0 + Duration::from_secs(10)),
            Duration::from_secs(20)
        );
        assert_eq!(record.remaining(t0 + WINDOW * 2), Duration::ZERO);
        assert!(record.describe().contains("Recreate x at dddddd"));
        let reset = UndoRecord::new(
            8,
            "Hard reset",
            UndoAction::MoveHeadBack {
                branch: None,
                from: oid('a'),
                to: oid('b'),
            },
            t0,
        );
        assert!(reset.describe().contains("bbbbbbb"));
    }
}
