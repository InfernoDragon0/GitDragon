//! Checking out a pull request: fetch its branch (or the host's `pull/<n>/head`-style ref
//! for forks) and switch to a local branch for it. Names and refs are validated before they
//! reach git; nothing here pushes.

use std::time::Duration;

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

// ---- a pull request's changes ----------------------------------------------------------------

/// The private namespace fetched pull requests land in: never a branch, never pushed, left out
/// of the commit graph (`log --exclude`) and ignored by the repository watcher.
pub const PR_REFS: &str = "refs/gitdragon/pr/";

/// How long the fetch of one pull request may take.
const PR_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Where pull request `number`'s fetched head (or merge preview) is kept.
pub fn pr_head_ref(number: u64) -> String {
    format!("{PR_REFS}{number}/head")
}

/// Where the tip of pull request `number`'s target branch is kept, as of its fetch.
pub fn pr_base_ref(number: u64) -> String {
    format!("{PR_REFS}{number}/base")
}

/// What a pull request's Changes compare: `base..head`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRange {
    /// The merge base of the target branch and the head; the merge preview's first parent.
    pub base: String,
    pub head: String,
    /// `head` is the host's merge preview (Azure DevOps `pull/<n>/merge`), not the head.
    pub merge_preview: bool,
}

/// What [`CliRepo::fetch_pr`] fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFetch<'a> {
    /// The remote the host matched.
    pub remote: &'a str,
    /// Host ref such as `pull/5/head` (see [`normalize_pull_ref`]).
    pub source_ref: &'a str,
    /// The branch the pull request merges into (not fetched for a merge preview).
    pub target_branch: &'a str,
    pub number: u64,
    pub merge_preview: bool,
    /// The target commit the host says the pull request is based on / merged into. When it is
    /// there, the base is its merge base with the head, so a merged pull request (whose head is
    /// an ancestor of the target tip) still diffs. Unknown or unusable: the target tip is used.
    pub base_sha: Option<&'a str>,
}

/// 40 or 64 hex digits.
fn is_full_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

impl CliRepo {
    /// The commit `rev` names, or `None` when it names none.
    async fn commit_of(&self, rev: &str) -> Result<Option<String>> {
        let cmd = GitCommand::read(self.root())
            .args(["rev-parse", "--verify", "--quiet"])
            .rev(&format!("{rev}^{{commit}}"))?;
        let out = self.runner().output(cmd).await?;
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.success() && is_full_sha(&sha)).then_some(sha))
    }

    /// The range of pull request `number` from its fetched refs, `None` when they are missing.
    async fn pr_range(
        &self,
        number: u64,
        merge_preview: bool,
        base_sha: Option<&str>,
    ) -> Result<Option<PrRange>> {
        let Some(head) = self.commit_of(&pr_head_ref(number)).await? else {
            return Ok(None);
        };
        let base = if merge_preview {
            self.commit_of(&format!("{head}^1")).await?
        } else {
            let Some(tip) = self.commit_of(&pr_base_ref(number)).await? else {
                return Ok(None);
            };
            // The host's base commit when the clone has it, else the fetched target tip.
            let hosted = match base_sha.filter(|s| is_full_sha(s)) {
                Some(sha) => self.commit_of(sha).await?,
                None => None,
            };
            let target = hosted.unwrap_or(tip);
            let cmd = GitCommand::read(self.root())
                .arg("merge-base")
                .rev(&target)?
                .rev(&head)?;
            let out = self.runner().output(cmd).await?;
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.success() || !is_full_sha(&sha) {
                return Err(GitError::InvalidArgument {
                    what: "pull request (no history shared with its target branch)",
                    value: number.to_string(),
                });
            }
            Some(sha)
        };
        Ok(base.map(|base| PrRange {
            base,
            head,
            merge_preview,
        }))
    }

    /// The range of pull request `number` from an earlier fetch, when its head is still
    /// `head_sha` (for a merge preview: the preview's second parent is). `None` means fetch.
    pub async fn cached_pr_range(
        &self,
        number: u64,
        head_sha: &str,
        merge_preview: bool,
        base_sha: Option<&str>,
    ) -> Result<Option<PrRange>> {
        if !is_full_sha(head_sha) {
            return Ok(None);
        }
        let fetched = if merge_preview {
            format!("{}^2", pr_head_ref(number))
        } else {
            pr_head_ref(number)
        };
        match self.commit_of(&fetched).await? {
            Some(sha) if sha.eq_ignore_ascii_case(head_sha) => {
                self.pr_range(number, merge_preview, base_sha).await
            }
            _ => Ok(None),
        }
    }

    /// Fetches a pull request's head (or merge preview) and its target branch's tip into
    /// [`PR_REFS`], touching no branch, remote-tracking ref, tag or `FETCH_HEAD`, and answers
    /// the range its Changes compare. Runs like auto-fetch (hardened, the app's askpass).
    pub async fn fetch_pr(&self, pr: &PrFetch<'_>) -> Result<PrRange> {
        let remote = self.check_remote_name(pr.remote).await?;
        let source = normalize_pull_ref(pr.source_ref)?;
        let mut cmd = GitCommand::write(self.root())
            .background()
            .timeout(Some(PR_FETCH_TIMEOUT))
            .args([
                "fetch",
                "--no-tags",
                "--no-progress",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--upload-pack=git-upload-pack",
                "--end-of-options",
            ])
            .rev(&remote)?
            .rev(&format!("+{source}:{}", pr_head_ref(pr.number)))?;
        if !pr.merge_preview {
            let target = self
                .check_ref_name("target branch", pr.target_branch, false)
                .await?;
            cmd = cmd.rev(&format!("+refs/heads/{target}:{}", pr_base_ref(pr.number)))?;
        }
        self.runner().run(cmd).await?;
        self.pr_range(pr.number, pr.merge_preview, pr.base_sha)
            .await?
            .ok_or(GitError::InvalidArgument {
                what: "pull request ref (nothing fetched)",
                value: source,
            })
    }

    /// Deletes the fetched pull requests whose head commit is older than `max_age` (best
    /// effort housekeeping); answers how many were dropped.
    pub async fn prune_pr_refs(&self, max_age: Duration) -> Result<usize> {
        let cmd = GitCommand::read(self.root())
            .args([
                "for-each-ref",
                "--format=%(refname)%09%(committerdate:unix)",
            ])
            .arg(PR_REFS);
        let out = self.runner().run(cmd).await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(max_age.as_secs());
        let listed = parse_pr_refs(&out);
        let stale: Vec<u64> = listed
            .iter()
            .filter(|r| r.head && r.date < cutoff)
            .map(|r| r.number)
            .collect();
        let mut script = String::new();
        for r in listed.iter().filter(|r| stale.contains(&r.number)) {
            script.push_str(&format!("delete {}\n", r.name));
        }
        if script.is_empty() {
            return Ok(0);
        }
        let cmd = GitCommand::write(self.root())
            .background()
            .args(["update-ref", "--no-deref", "--stdin"])
            .stdin(script.into_bytes());
        self.runner().run(cmd).await?;
        Ok(stale.len())
    }
}

/// One line of `for-each-ref --format=%(refname)%09%(committerdate:unix) refs/gitdragon/pr/`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrRefLine {
    name: String,
    number: u64,
    head: bool,
    date: u64,
}

/// Lines naming anything but `refs/gitdragon/pr/<n>/{head,base}` are skipped.
fn parse_pr_refs(out: &[u8]) -> Vec<PrRefLine> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|line| {
            let (name, date) = line.split_once('\t')?;
            let (number, kind) = name.strip_prefix(PR_REFS)?.split_once('/')?;
            let head = match kind {
                "head" => true,
                "base" => false,
                _ => return None,
            };
            Some(PrRefLine {
                name: name.to_string(),
                number: number.parse().ok()?,
                head,
                date: date.trim().parse().unwrap_or(0),
            })
        })
        .collect()
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

    #[test]
    fn pr_refs_listing() {
        let out = b"refs/gitdragon/pr/5/head\t100\nrefs/gitdragon/pr/5/base\t50\n\
refs/gitdragon/pr/x/head\t1\nrefs/gitdragon/pr/6/other\t1\nrefs/heads/main\t1\n";
        let refs = parse_pr_refs(out);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].number, 5);
        assert!(refs[0].head && !refs[1].head);
        assert_eq!(refs[0].date, 100);
        assert_eq!(pr_head_ref(5), "refs/gitdragon/pr/5/head");
        assert_eq!(pr_base_ref(5), "refs/gitdragon/pr/5/base");
        assert!(is_full_sha(&"a".repeat(40)) && !is_full_sha("abc"));
    }
}
