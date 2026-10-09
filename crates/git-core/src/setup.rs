//! Creating repositories: `git init` and `git clone` with progress.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};
use crate::progress::Progress;
use crate::runner::{GitCommand, GitRunner};

/// Transports that run arbitrary commands; never allowed from the UI.
const BLOCKED_TRANSPORTS: &[&str] = &["ext::", "fd::"];

/// Checks a clone URL / path typed by the user. Accepts `https://`, `ssh://`, `git://`,
/// `file://`, scp-like `git@host:path` and local paths.
pub fn validate_clone_url(url: &str) -> Result<()> {
    let invalid = || GitError::InvalidArgument {
        what: "repository URL",
        value: url.to_string(),
    };
    let url = url.trim();
    if url.is_empty()
        || url.starts_with('-')
        || url.chars().any(char::is_control)
        || BLOCKED_TRANSPORTS
            .iter()
            .any(|t| url.to_ascii_lowercase().starts_with(t))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Folder name git would pick: last path segment without `.git` (`https://x/y/repo.git` -> `repo`).
pub fn default_dir_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// `git init` in `dir` (created if missing). Returns the worktree root.
pub async fn init(runner: &GitRunner, dir: &Path) -> Result<PathBuf> {
    init_with_branch(runner, dir, None).await
}

/// [`init`] with the first branch named `branch` (`--initial-branch`); `None` leaves it to
/// `init.defaultBranch` / git's default. The name is checked like any branch name.
pub async fn init_with_branch(
    runner: &GitRunner,
    dir: &Path,
    branch: Option<&str>,
) -> Result<PathBuf> {
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    let mut cmd = GitCommand::write(dir).args(["init", "--quiet"]);
    if let Some(branch) = branch {
        crate::runner::validate_rev(branch).map_err(|_| GitError::InvalidArgument {
            what: "branch name",
            value: branch.to_string(),
        })?;
        // `check-ref-format` needs no repository; run it in the folder that exists already.
        let probe_dir = dir
            .ancestors()
            .find(|d| d.is_dir())
            .unwrap_or(Path::new("."));
        let check = runner
            .output(
                GitCommand::read(probe_dir)
                    .args(["check-ref-format", "--branch"])
                    .arg(branch),
            )
            .await?;
        if !check.success() {
            return Err(GitError::InvalidArgument {
                what: "branch name",
                value: branch.to_string(),
            });
        }
        cmd = cmd.arg(format!("--initial-branch={branch}"));
    }
    std::fs::create_dir_all(dir)?;
    runner.run(cmd).await?;
    Ok(dir.to_path_buf())
}

/// `git clone --progress <url> <dest>`; `dest` must not exist or be an empty directory.
/// Drop the future to cancel (git is killed; the half-cloned folder is removed).
pub async fn clone(
    runner: &GitRunner,
    url: &str,
    dest: &Path,
    on_progress: impl FnMut(Progress) + Send,
) -> Result<PathBuf> {
    validate_clone_url(url)?;
    let parent = dest.parent().ok_or_else(|| GitError::InvalidArgument {
        what: "clone destination",
        value: dest.display().to_string(),
    })?;
    if dest.exists() && std::fs::read_dir(dest)?.next().is_some() {
        return Err(GitError::InvalidArgument {
            what: "clone destination (not empty)",
            value: dest.display().to_string(),
        });
    }
    std::fs::create_dir_all(parent)?;
    let created = !dest.exists();
    let cleanup = RemoveOnDrop {
        path: created.then(|| dest.to_path_buf()),
    };

    let cmd = GitCommand::write(parent)
        .args([
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "protocol.fd.allow=never",
        ])
        .args(["clone", "--progress", "--"])
        .user_arg("repository URL", url.trim())?
        .user_arg("clone destination", dest.as_os_str())?;
    runner.run_with_progress(cmd, on_progress).await?;
    cleanup.disarm();
    Ok(dest.to_path_buf())
}

/// Deletes a folder the clone created unless the clone finished.
struct RemoveOnDrop {
    path: Option<PathBuf>,
}

impl RemoveOnDrop {
    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(err) = std::fs::remove_dir_all(&path)
        {
            tracing::warn!(path = %path.display(), %err, "could not remove partial clone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation() {
        for ok in [
            "https://github.com/a/b.git",
            "git@github.com:a/b.git",
            "ssh://git@host:22/a/b",
            "C:\\repos\\my repo",
            "/srv/git/x.git",
            "file:///tmp/x",
        ] {
            assert!(validate_clone_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "  ",
            "--upload-pack=touch /tmp/pwn",
            "ext::sh -c touch% /tmp/pwn",
            "EXT::sh",
            "fd::3",
            "https://x\n/y",
        ] {
            assert!(validate_clone_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn dir_names() {
        assert_eq!(
            default_dir_name("https://github.com/a/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            default_dir_name("git@github.com:a/repo.git").as_deref(),
            Some("repo")
        );
        assert_eq!(
            default_dir_name("https://host/a/repo/").as_deref(),
            Some("repo")
        );
        assert_eq!(default_dir_name("C:\\x\\proj").as_deref(), Some("proj"));
        assert_eq!(default_dir_name("https://host/..").as_deref(), None);
    }

    #[test]
    fn init_then_clone_locally_with_progress() {
        let runner = GitRunner::detect().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src repo");
        async_io::block_on(init(&runner, &src)).unwrap();
        std::fs::write(src.join("f.txt"), "x").unwrap();
        for args in [
            &["add", "f.txt"][..],
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "m",
            ],
        ] {
            async_io::block_on(runner.run(GitCommand::write(&src).args(args))).unwrap();
        }

        let dest = tmp.path().join("out").join("clone");
        let mut updates = Vec::new();
        // `file://` forces the pack protocol, which reports progress.
        let url = format!("file:///{}", src.display().to_string().replace('\\', "/"));
        async_io::block_on(clone(&runner, &url, &dest, |p| updates.push(p))).unwrap();
        assert!(dest.join("f.txt").is_file());
        assert!(!updates.is_empty(), "no progress lines");

        // Non-empty destination is refused and left alone.
        let err = async_io::block_on(clone(&runner, &url, &dest, |_| {})).unwrap_err();
        assert!(matches!(err, GitError::InvalidArgument { .. }));
        assert!(dest.join("f.txt").is_file());

        // A failed clone removes the folder it created.
        let missing = tmp.path().join("nope");
        let dest2 = tmp.path().join("clone2");
        let bad = format!(
            "file:///{}",
            missing.display().to_string().replace('\\', "/")
        );
        assert!(async_io::block_on(clone(&runner, &bad, &dest2, |_| {})).is_err());
        assert!(!dest2.exists());
    }
}
