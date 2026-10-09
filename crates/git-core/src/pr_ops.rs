//! Checking out a pull request: fetch its branch (or the host's `pull/<n>/head`-style ref
//! for forks) and switch to a local branch for it. Names and refs are validated before they
//! reach git; nothing here pushes.

use crate::cli::CliRepo;
use crate::error::{GitError, Result};
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// The local branch a fetched pull request head lands on: `pr/<number>`.
pub fn pr_branch_name(number: u64) -> String {
    format!("pr/{number}")
}

/// A host-provided ref such as `pull/5/head` or `merge-requests/5/head`, as a full ref
/// (`refs/pull/5/head`). Only plain path characters are accepted.
pub fn normalize_pull_ref(head_ref: &str) -> Result<String> {
    let trimmed = head_ref.strip_prefix("refs/").unwrap_or(head_ref);
    let plain = !trimmed.is_empty()
        && trimmed.len() <= 200
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        && !trimmed.starts_with(['/', '-', '.'])
        && !trimmed.ends_with(['/', '.'])
        && !trimmed.contains("//")
        && !trimmed.contains("..")
        && !trimmed.contains("/.");
    if !plain {
        return Err(GitError::InvalidArgument {
            what: "pull request ref",
            value: head_ref.to_string(),
        });
    }
    Ok(format!("refs/{trimmed}"))
}

impl CliRepo {
    async fn local_branch_exists(&self, name: &str) -> Result<bool> {
        let cmd = GitCommand::read(self.root())
            .args(["rev-parse", "--verify", "--quiet"])
            .rev(&format!("refs/heads/{name}"))?;
        Ok(self.runner().output(cmd).await?.success())
    }

    async fn fetch_refspec(&self, remote: &str, refspec: &str) -> Result<()> {
        let cmd = GitCommand::write(self.root())
            .args(["fetch", "--no-progress", "--end-of-options"])
            .rev(remote)?
            .rev(refspec)?;
        self.runner().run(cmd).await.map(drop)
    }

    /// Fetches `branch` of `remote` and switches to the local branch of that name: created
    /// tracking `remote/branch` when missing, simply switched to when it exists. For pull
    /// requests whose source branch lives in the same repository. Returns the local name.
    pub async fn checkout_remote_branch(&self, remote: &str, branch: &str) -> Result<String> {
        let remote = self.check_remote_name(remote).await?;
        let branch = self.check_branch_name(branch).await?;
        self.fetch_refspec(
            &remote,
            &format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}"),
        )
        .await?;
        if self.local_branch_exists(&branch).await? {
            self.switch_branch(&branch).await?;
        } else {
            let upstream = format!("{remote}/{branch}");
            self.create_branch(&branch, &upstream, true).await?;
            self.set_upstream(&branch, Some(&upstream)).await?;
        }
        Ok(branch)
    }

    /// Fetches the head of pull request `number` through `head_ref` (see
    /// [`normalize_pull_ref`]), the way to reach a fork's branch, and switches to the local
    /// branch `pr/<number>`: created when missing, otherwise fast-forwarded to the new head
    /// (it fails, changing nothing, if the pull request was force-pushed over local commits).
    /// Returns the local name.
    pub async fn checkout_pull_ref(
        &self,
        remote: &str,
        head_ref: &str,
        number: u64,
    ) -> Result<String> {
        let remote = self.check_remote_name(remote).await?;
        let source = normalize_pull_ref(head_ref)?;
        let local = self.check_branch_name(&pr_branch_name(number)).await?;
        let tracking = format!("{remote}/{local}");
        self.fetch_refspec(&remote, &format!("+{source}:refs/remotes/{tracking}"))
            .await?;
        if self.local_branch_exists(&local).await? {
            self.switch_branch(&local).await?;
            let cmd = GitCommand::write(self.root())
                .args(["merge", "--ff-only", "--no-edit", "--end-of-options"])
                .rev(&tracking)?;
            self.runner().run(cmd).await?;
        } else {
            self.create_branch(&local, &tracking, true).await?;
        }
        Ok(local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_refs_are_plain_paths() {
        assert_eq!(
            normalize_pull_ref("pull/5/head").unwrap(),
            "refs/pull/5/head"
        );
        assert_eq!(
            normalize_pull_ref("refs/merge-requests/12/head").unwrap(),
            "refs/merge-requests/12/head"
        );
        for bad in [
            "",
            "-x",
            "/pull/5/head",
            "pull/../x",
            "pull//5",
            "a b",
            "pull/5/head:x",
            "pull/5/",
            "pull/.x",
            "x\n",
            "a*b",
            "a:b",
            "~x",
        ] {
            assert!(normalize_pull_ref(bad).is_err(), "{bad:?}");
        }
        assert_eq!(pr_branch_name(5), "pr/5");
    }
}
