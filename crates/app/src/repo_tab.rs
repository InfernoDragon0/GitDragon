//! One open repository: its git handle, latest status / refs, and the file watcher that keeps
//! them fresh. All git work runs on the background executor.

use std::collections::HashMap;
use std::path::PathBuf;

use bstr::BString;
use git_core::lfs::{LfsAttrs, LfsStatus, Lock};
use git_core::parse::refs::{RefEntry, Remote, Stash};
use git_core::parse::status::Status;
use git_core::watch::{self, ChangeSet, RepoWatcher};
use git_core::{CliRepo, GitRepo as _, GitRunner, Submodule};
use gpui_kit::{Context, SharedString, Task};

use crate::state::repo_name;

/// Wait after opening before background upkeep starts.
const MAINTENANCE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

pub struct RepoTab {
    pub path: PathBuf,
    pub name: SharedString,
    pub repo: Option<CliRepo>,
    pub status: Option<Status>,
    pub refs: Vec<RefEntry>,
    pub stashes: Vec<Stash>,
    pub remotes: Vec<Remote>,
    pub submodules: Vec<Submodule>,
    /// Git LFS: installed? used by this repo? `None` until checked on open.
    pub lfs: Option<LfsStatus>,
    /// LFS attributes of the paths in [`Self::status`] (only those that have any).
    pub lfs_attrs: HashMap<BString, LfsAttrs>,
    /// LFS locks from the server, loaded on open and after lock / unlock (network).
    pub locks: Vec<Lock>,
    pub locks_error: Option<SharedString>,
    locks_task: Option<Task<()>>,
    pub loading: bool,
    /// Last failed user operation (sidebar, toolbar, graph menu); cleared by the next one.
    pub op_error: Option<SharedString>,
    /// Label of the running user operation, e.g. `Pushing main`.
    pub op_label: Option<SharedString>,
    /// The running user operation; dropping it kills git.
    op_task: Option<Task<()>>,
    /// Bumped on every successful refresh, so views can tell the status changed.
    pub generation: u64,
    pub error: Option<SharedString>,
    runner: GitRunner,
    watcher: Option<RepoWatcher>,
    /// Replaced on every refresh; dropping the old task kills its git processes.
    refresh_task: Option<Task<()>>,
    ignore_task: Option<Task<()>>,
    /// Background upkeep (commit-graph write); dropped with the tab, which kills git.
    maintenance_task: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

impl RepoTab {
    pub fn new(path: PathBuf, runner: GitRunner, cx: &mut Context<Self>) -> Self {
        let mut tab = Self {
            name: repo_name(&path).into(),
            path,
            repo: None,
            status: None,
            refs: Vec::new(),
            stashes: Vec::new(),
            remotes: Vec::new(),
            submodules: Vec::new(),
            lfs: None,
            lfs_attrs: HashMap::new(),
            locks: Vec::new(),
            locks_error: None,
            locks_task: None,
            loading: true,
            op_error: None,
            op_label: None,
            op_task: None,
            generation: 0,
            error: None,
            runner,
            watcher: None,
            refresh_task: None,
            ignore_task: None,
            maintenance_task: None,
            _tasks: Vec::new(),
        };
        tab.open(cx);
        tab
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        let path = self.path.clone();
        let (tx, rx) = async_channel::unbounded::<ChangeSet>();
        let task = cx.spawn(async move |this, cx| {
            // Opening and starting the watcher touch the filesystem: keep them off the UI thread.
            let opened = cx
                .background_executor()
                .spawn(async move {
                    let repo = CliRepo::open(runner.clone(), &path).await?;
                    let git_dir = watch::git_dir(&runner, repo.root()).await?;
                    let ignored = watch::ignored_dirs(&runner, repo.root())
                        .await
                        .unwrap_or_default();
                    let watcher = RepoWatcher::new(repo.root(), &git_dir, ignored, move |change| {
                        _ = tx.try_send(change);
                    })
                    .inspect_err(|err| {
                        // Still usable without live updates (F5 refreshes).
                        tracing::warn!(path = %path.display(), %err, "file watcher failed");
                    })
                    .ok();
                    Ok::<_, git_core::GitError>((repo, watcher))
                })
                .await;
            let started = this.update(cx, |tab, cx| match opened {
                Ok((repo, watcher)) => {
                    tab.watcher = watcher;
                    tab.repo = Some(repo);
                    tab.refresh(cx);
                    tab.load_lfs(cx);
                    tab.ensure_commit_graph(cx);
                    true
                }
                Err(err) => {
                    tab.loading = false;
                    tab.error = Some(err.to_string().into());
                    cx.notify();
                    false
                }
            });
            if !matches!(started, Ok(true)) {
                return;
            }
            while let Ok(mut change) = rx.recv().await {
                // Coalesce whatever queued up while we were busy.
                while let Ok(more) = rx.try_recv() {
                    merge(&mut change, more);
                }
                if this
                    .update(cx, |tab, cx| tab.on_change(change, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self._tasks.push(task);
    }

    /// Checks git-lfs and the repo's LFS use; when it uses LFS, refreshes again (now with LFS
    /// attributes) and, for lockable files, loads the locks.
    fn load_lfs(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let lfs = cx
                .background_executor()
                .spawn(async move { repo.lfs_status().await })
                .await
                .unwrap_or_default();
            _ = this.update(cx, |tab, cx| {
                let (used, lockable) = (lfs.used, lfs.lockable && lfs.version.is_some());
                tab.lfs = Some(lfs);
                if used {
                    tab.refresh(cx);
                } else {
                    cx.notify();
                }
                if lockable {
                    tab.refresh_locks(cx);
                }
            });
        });
        self._tasks.push(task);
    }

    /// Does this repo store files in LFS?
    pub fn uses_lfs(&self) -> bool {
        self.lfs.as_ref().is_some_and(|l| l.used)
    }

    /// Reloads the LFS locks from the server.
    pub fn refresh_locks(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.locks_task = Some(cx.spawn(async move |this, cx| {
            let locks = cx
                .background_executor()
                .spawn(async move { repo.lfs_locks().await })
                .await;
            _ = this.update(cx, |tab, cx| {
                tab.locks_task = None;
                match locks {
                    Ok(locks) => {
                        tab.locks = locks;
                        tab.locks_error = None;
                    }
                    Err(err) => tab.locks_error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    /// The lock on `path`, if any.
    pub fn lock_on(&self, path: &[u8]) -> Option<&Lock> {
        self.locks.iter().find(|l| l.path == path)
    }

    /// Locks (or unlocks) an LFS file, then reloads the locks. `force` breaks someone
    /// else's lock.
    pub fn set_lock(&mut self, path: BString, lock: bool, force: bool, cx: &mut Context<Self>) {
        let name = String::from_utf8_lossy(&path).into_owned();
        let label = if lock {
            format!("Locking {name}")
        } else {
            format!("Unlocking {name}")
        };
        self.run_op_then(
            label,
            cx,
            move |repo| async move {
                if lock {
                    repo.lfs_lock(&path).await
                } else {
                    repo.lfs_unlock(&path, force).await
                }
            },
            |tab, cx| tab.refresh_locks(cx),
        );
    }

    /// Writes / extends the commit-graph file so the graph streams its first rows at once.
    /// Starts after a short delay so it does not compete with the first status and log.
    fn ensure_commit_graph(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let path = self.path.clone();
        self.maintenance_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(MAINTENANCE_DELAY).await;
            let started = std::time::Instant::now();
            let result = cx
                .background_executor()
                .spawn(async move { repo.ensure_commit_graph().await })
                .await;
            match result {
                Ok(outcome) => tracing::info!(
                    path = %path.display(),
                    ?outcome,
                    elapsed = ?started.elapsed(),
                    "commit-graph"
                ),
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "commit-graph write failed")
                }
            }
        }));
    }

    fn on_change(&mut self, change: ChangeSet, cx: &mut Context<Self>) {
        if change.ignore_rules {
            self.refresh_ignored(cx);
        }
        self.refresh(cx);
    }

    fn refresh_ignored(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.repo.as_ref().map(|r| r.root().to_path_buf()) else {
            return;
        };
        let runner = self.runner.clone();
        self.ignore_task = Some(cx.spawn(async move |this, cx| {
            let ignored = cx
                .background_executor()
                .spawn(async move { watch::ignored_dirs(&runner, &root).await })
                .await;
            if let Ok(ignored) = ignored {
                _ = this.update(cx, |tab, _| {
                    if let Some(watcher) = &tab.watcher {
                        watcher.set_ignored(ignored);
                    }
                });
            }
        }));
    }

    /// Reloads status and refs.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.loading = true;
        cx.notify();
        let lfs = self.uses_lfs();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = repo.status().await?;
                    // Optional: badges only.
                    let lfs_attrs = if lfs {
                        let paths: Vec<BString> =
                            status.entries.iter().map(|e| e.path.clone()).collect();
                        repo.lfs_attrs(&paths).await.unwrap_or_default()
                    } else {
                        HashMap::new()
                    };
                    let refs = repo.refs().await?;
                    let stashes = repo.stashes().await?;
                    let remotes = repo.remotes().await?;
                    // Optional: a broken .gitmodules must not hide the rest.
                    let submodules = repo.submodules().await.unwrap_or_default();
                    Ok::<_, git_core::GitError>((
                        status, refs, stashes, remotes, submodules, lfs_attrs,
                    ))
                })
                .await;
            _ = this.update(cx, |tab, cx| {
                tab.loading = false;
                match result {
                    Ok((status, refs, stashes, remotes, submodules, lfs_attrs)) => {
                        tab.status = Some(status);
                        tab.lfs_attrs = lfs_attrs;
                        tab.refs = refs;
                        tab.stashes = stashes;
                        tab.remotes = remotes;
                        tab.submodules = submodules;
                        tab.generation += 1;
                        tab.error = None;
                    }
                    Err(err) => tab.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    pub fn is_busy(&self) -> bool {
        self.op_task.is_some()
    }

    /// Runs a user operation (one at a time per tab), then refreshes. Its error stays in
    /// [`Self::op_error`] until the next operation or [`Self::clear_op_error`].
    pub fn run_op<F, Fut>(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>, op: F)
    where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        self.run_op_then(label, cx, op, |_, _| {});
    }

    /// [`Self::run_op`], then `then` (whether or not it failed) before the refresh.
    pub fn run_op_then<F, Fut>(
        &mut self,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        op: F,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) where
        F: FnOnce(CliRepo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = git_core::Result<()>> + Send + 'static,
    {
        if self.op_task.is_some() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.op_error = None;
        self.op_label = Some(label.into());
        let job = cx.background_executor().spawn(op(repo));
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            _ = this.update(cx, |tab, cx| {
                tab.op_task = None;
                tab.op_label = None;
                if let Err(err) = result {
                    tab.op_error = Some(err.to_string().into());
                }
                then(tab, cx);
                tab.refresh(cx);
            });
        }));
        cx.notify();
    }

    /// Stops the running operation (its git process is killed).
    pub fn cancel_op(&mut self, cx: &mut Context<Self>) {
        if self.op_task.take().is_some() {
            self.op_label = None;
            self.op_error = Some("Cancelled.".into());
            cx.notify();
        }
    }

    pub fn clear_op_error(&mut self, cx: &mut Context<Self>) {
        if self.op_error.take().is_some() {
            cx.notify();
        }
    }

    /// `main · 3 changed`-style summary for placeholders and the tab tooltip.
    pub fn summary(&self) -> String {
        if let Some(err) = &self.error {
            return err.to_string();
        }
        let Some(status) = &self.status else {
            return "Loading…".into();
        };
        let branch = status
            .branch
            .head
            .clone()
            .unwrap_or_else(|| "detached HEAD".into());
        let changed = status.entries.len();
        let mut out = format!("{branch} · {changed} changed");
        if status.branch.ahead > 0 || status.branch.behind > 0 {
            out.push_str(&format!(
                " · ↑{} ↓{}",
                status.branch.ahead, status.branch.behind
            ));
        }
        out
    }
}

fn merge(into: &mut ChangeSet, more: ChangeSet) {
    into.worktree |= more.worktree;
    into.index |= more.index;
    into.refs |= more.refs;
    into.head |= more.head;
    into.operation |= more.operation;
    into.ignore_rules |= more.ignore_rules;
    into.truncated |= more.truncated;
    into.paths.extend(more.paths);
}
