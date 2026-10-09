//! One open repository: its git handle, latest status / refs, and the file watcher that keeps
//! them fresh. All git work runs on the background executor.

use std::path::PathBuf;

use git_core::parse::refs::RefEntry;
use git_core::parse::status::Status;
use git_core::watch::{self, ChangeSet, RepoWatcher};
use git_core::{CliRepo, GitRepo as _, GitRunner};
use gpui_kit::{Context, SharedString, Task};

use crate::state::repo_name;

pub struct RepoTab {
    pub path: PathBuf,
    pub name: SharedString,
    pub repo: Option<CliRepo>,
    pub status: Option<Status>,
    pub refs: Vec<RefEntry>,
    pub loading: bool,
    pub error: Option<SharedString>,
    runner: GitRunner,
    watcher: Option<RepoWatcher>,
    /// Replaced on every refresh; dropping the old task kills its git processes.
    refresh_task: Option<Task<()>>,
    ignore_task: Option<Task<()>>,
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
            loading: true,
            error: None,
            runner,
            watcher: None,
            refresh_task: None,
            ignore_task: None,
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
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = repo.status().await?;
                    let refs = repo.refs().await?;
                    Ok::<_, git_core::GitError>((status, refs))
                })
                .await;
            _ = this.update(cx, |tab, cx| {
                tab.loading = false;
                match result {
                    Ok((status, refs)) => {
                        tab.status = Some(status);
                        tab.refs = refs;
                        tab.error = None;
                    }
                    Err(err) => tab.error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
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
