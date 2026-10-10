//! Git LFS awareness. Clone / checkout / pull / push / commit already go through LFS because
//! git runs its filters (decision D1: the git CLI does the work); this module adds what the UI
//! shows on top: is git-lfs installed, which paths LFS stores and which are lockable, pointer
//! diffs as sizes, locks, and `git lfs fetch` / `prune`.

use std::collections::HashMap;

use bstr::{BString, ByteSlice as _};
use serde::Deserialize;

use crate::cli::CliRepo;
use crate::error::{GitError, Result};
use crate::parse::diff::{FileDiff, LineKind};
use crate::repo::GitRepo as _;
use crate::runner::GitCommand;

/// What a repository needs from LFS and what this machine has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LfsStatus {
    /// `git lfs version` output (`git-lfs/3.3.0 (...)`); `None` when git-lfs is not installed.
    pub version: Option<String>,
    /// The root `.gitattributes` routes files through LFS (`filter=lfs`).
    pub used: bool,
    /// Patterns marked `lockable` there.
    pub lockable: bool,
}

impl LfsStatus {
    /// The repo stores files in LFS but this machine cannot fetch them: checkouts get pointer
    /// files instead of content.
    pub fn missing(&self) -> bool {
        self.used && self.version.is_none()
    }
}

/// LFS attributes of one path (`git check-attr filter lockable`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LfsAttrs {
    pub lfs: bool,
    pub lockable: bool,
}

/// An LFS pointer file: what git stores in place of the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// `sha256:<hex>`.
    pub oid: String,
    pub size: u64,
}

/// A pointer change in a diff: either side may be absent (added / deleted file) or not a
/// pointer (the file moved into or out of LFS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerChange {
    pub old: Option<Pointer>,
    pub new: Option<Pointer>,
}

/// A lock on the LFS server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub id: String,
    pub path: BString,
    pub owner: String,
    /// RFC 3339, as the server sent it.
    pub locked_at: String,
    /// Held by the signed-in user (from `--verify`).
    pub ours: bool,
}

/// Parses a pointer file (`version https://git-lfs.github.com/spec/v1`, `oid sha256:…`,
/// `size N`). `None` for anything else.
pub fn parse_pointer(text: &[u8]) -> Option<Pointer> {
    // Pointers are tiny; real content never starts with the version line.
    if text.len() > 1024 {
        return None;
    }
    let mut lines = text.lines().map(|l| l.trim());
    let version = lines.next()?;
    if !version.starts_with(b"version https://git-lfs.github.com/spec/") {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for line in lines {
        if let Some(v) = line.strip_prefix(b"oid ") {
            oid = Some(v.to_str().ok()?.to_string());
        } else if let Some(v) = line.strip_prefix(b"size ") {
            size = Some(v.to_str().ok()?.parse().ok()?);
        }
    }
    Some(Pointer {
        oid: oid?,
        size: size?,
    })
}

/// The pointer change a text diff of an LFS file shows, rebuilt from its hunks (pointer files
/// are a few lines, so the hunk holds the whole file). `None` when neither side is a pointer.
pub fn pointer_change(diff: &FileDiff) -> Option<PointerChange> {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for line in diff.hunks.iter().flat_map(|h| &h.lines) {
        let side: &mut [&mut Vec<u8>] = match line.kind {
            LineKind::Context => &mut [&mut old, &mut new],
            LineKind::Deleted => &mut [&mut old],
            LineKind::Added => &mut [&mut new],
            LineKind::NoNewlineAtEof => &mut [],
        };
        for text in side {
            text.extend_from_slice(&line.content);
            text.push(b'\n');
        }
    }
    let change = PointerChange {
        old: parse_pointer(&old),
        new: parse_pointer(&new),
    };
    (change.old.is_some() || change.new.is_some()).then_some(change)
}

/// One `GIT_LFS_PROGRESS` line (`download 3/12 1048576/2097152 Content/Map.umap`) as the
/// operation strip shows it: `LFS download 3/12 · Map.umap 50%`.
pub fn describe_progress(line: &str) -> Option<String> {
    let mut parts = line.trim().splitn(4, ' ');
    let direction = parts.next()?;
    let files = parts.next()?;
    let bytes = parts.next()?;
    let name = parts.next().unwrap_or_default();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (done, total) = bytes.split_once('/')?;
    let (done, total): (u64, u64) = (done.parse().ok()?, total.parse().ok()?);
    let percent = (done * 100).checked_div(total).unwrap_or(100);
    Some(format!("LFS {direction} {files} \u{b7} {name} {percent}%"))
}

/// `12.0 MB`-style size (decimal units, as git-lfs prints them).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit + 1 < UNITS.len() {
        value /= 1000.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Does `.gitattributes` text route anything through LFS / mark anything lockable?
fn scan_attributes(text: &[u8]) -> (bool, bool) {
    let mut used = false;
    let mut lockable = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(b"#") {
            continue;
        }
        let mut words = line.split_str(" ").filter(|w| !w.is_empty()).skip(1);
        for attr in &mut words {
            match attr {
                b"filter=lfs" => used = true,
                b"lockable" => lockable = true,
                _ => {}
            }
        }
    }
    (used, lockable)
}

#[derive(Deserialize)]
struct JsonOwner {
    name: String,
}

#[derive(Deserialize)]
struct JsonLock {
    id: String,
    path: String,
    owner: Option<JsonOwner>,
    #[serde(default)]
    locked_at: String,
}

#[derive(Deserialize)]
struct JsonVerify {
    #[serde(default)]
    ours: Vec<JsonLock>,
    #[serde(default)]
    theirs: Vec<JsonLock>,
}

/// `git lfs locks --verify --json`: `{"ours": [...], "theirs": [...]}`; plain `--json` (no
/// `--verify`, older servers) is a bare array of other people's locks.
fn parse_locks(json: &[u8]) -> Result<Vec<Lock>> {
    let bad = |err: serde_json::Error| GitError::InvalidArgument {
        what: "git lfs locks output",
        value: err.to_string(),
    };
    let lock = |l: JsonLock, ours: bool| Lock {
        id: l.id,
        path: BString::from(l.path),
        owner: l.owner.map(|o| o.name).unwrap_or_default(),
        locked_at: l.locked_at,
        ours,
    };
    let text = json.trim();
    if text.starts_with(b"[") {
        let all: Vec<JsonLock> = serde_json::from_slice(text).map_err(bad)?;
        return Ok(all.into_iter().map(|l| lock(l, false)).collect());
    }
    let verify: JsonVerify = serde_json::from_slice(text).map_err(bad)?;
    Ok(verify
        .ours
        .into_iter()
        .map(|l| lock(l, true))
        .chain(verify.theirs.into_iter().map(|l| lock(l, false)))
        .collect())
}

/// `git check-attr -z` output: `path NUL attr NUL value NUL`, repeated.
fn parse_check_attr(output: &[u8]) -> HashMap<BString, LfsAttrs> {
    let mut found: HashMap<BString, LfsAttrs> = HashMap::new();
    let fields: Vec<&[u8]> = output.split(|&b| b == 0).collect();
    for [path, attr, value] in fields.as_chunks::<3>().0 {
        let (path, attr, value) = (*path, *attr, *value);
        let entry = found.entry(BString::from(path)).or_default();
        match attr {
            b"filter" if value == b"lfs" => entry.lfs = true,
            b"lockable" if value == b"set" => entry.lockable = true,
            _ => {}
        }
    }
    found
}

impl CliRepo {
    /// Is git-lfs installed, and does the repo use it (root `.gitattributes`)?
    pub async fn lfs_status(&self) -> Result<LfsStatus> {
        let version = self
            .runner()
            .run(GitCommand::read(self.root()).args(["lfs", "version"]))
            .await
            .ok()
            .map(|out| String::from_utf8_lossy(&out).trim().to_string())
            .filter(|v| v.starts_with("git-lfs/"));
        let (used, lockable) = std::fs::read(self.root().join(".gitattributes"))
            .map(|text| scan_attributes(&text))
            .unwrap_or_default();
        Ok(LfsStatus {
            version,
            used,
            lockable,
        })
    }

    /// LFS attributes of `paths` (only paths with any are returned).
    pub async fn lfs_attrs(&self, paths: &[BString]) -> Result<HashMap<BString, LfsAttrs>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        // Paths on stdin: thousands of them (an Unreal commit) overflow the Windows command
        // line (32k characters).
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path);
            input.push(0);
        }
        let cmd = GitCommand::read(self.root())
            .args(["check-attr", "-z", "--stdin", "filter", "lockable"])
            .stdin(input);
        let out = self.runner().run(cmd).await?;
        let mut attrs = parse_check_attr(&out);
        attrs.retain(|_, a| a.lfs || a.lockable);
        Ok(attrs)
    }

    /// Locks on the LFS server (network; asks for credentials like a push).
    pub async fn lfs_locks(&self) -> Result<Vec<Lock>> {
        let cmd = GitCommand::write(self.root())
            .background()
            .args(["lfs", "locks", "--verify", "--json"]);
        match self.runner().run(cmd).await {
            Ok(out) => parse_locks(&out),
            // Servers without lock verification: fall back to the plain list.
            Err(_) => {
                let cmd = GitCommand::write(self.root())
                    .background()
                    .args(["lfs", "locks", "--json"]);
                parse_locks(&self.runner().run(cmd).await?)
            }
        }
    }

    pub async fn lfs_lock(&self, path: &BString) -> Result<()> {
        crate::ops::validate_repo_path(path)?;
        let cmd = GitCommand::write(self.root())
            .args(["lfs", "lock"])
            .paths([path]);
        self.runner().run(cmd).await.map(drop)
    }

    /// `force` breaks someone else's lock (the server decides whether you may).
    pub async fn lfs_unlock(&self, path: &BString, force: bool) -> Result<()> {
        crate::ops::validate_repo_path(path)?;
        let mut cmd = GitCommand::write(self.root()).args(["lfs", "unlock"]);
        if force {
            cmd = cmd.arg("--force");
        }
        self.runner().run(cmd.paths([path])).await.map(drop)
    }

    /// Downloads the LFS objects of the current checkout (`git lfs fetch`, then `checkout` to
    /// replace pointer files with content).
    pub async fn lfs_pull(&self) -> Result<()> {
        let cmd = GitCommand::write(self.root()).args(["lfs", "pull"]);
        self.runner().run(cmd).await.map(drop)
    }

    /// Deletes local LFS objects that are old and already pushed (`git lfs prune`).
    pub async fn lfs_prune(&self) -> Result<()> {
        let cmd = GitCommand::write(self.root()).args(["lfs", "prune"]);
        self.runner().run(cmd).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::diff::parse;

    const OLD: &str =
        "version https://git-lfs.github.com/spec/v1\noid sha256:aaaa\nsize 12000000\n";

    #[test]
    fn pointers() {
        assert_eq!(
            parse_pointer(OLD.as_bytes()),
            Some(Pointer {
                oid: "sha256:aaaa".into(),
                size: 12_000_000
            })
        );
        assert_eq!(parse_pointer(b"hello\nworld\n"), None);
        assert_eq!(
            parse_pointer(b"version https://git-lfs.github.com/spec/v1\nsize 5\n"),
            None
        );
    }

    #[test]
    fn pointer_change_from_a_diff() {
        let patch = "diff --git a/Map.umap b/Map.umap\nindex 1..2 100644\n--- a/Map.umap\n+++ b/Map.umap\n@@ -1,3 +1,3 @@\n version https://git-lfs.github.com/spec/v1\n-oid sha256:aaaa\n-size 12000000\n+oid sha256:bbbb\n+size 14200000\n";
        let diff = &parse(patch.as_bytes()).unwrap()[0];
        let change = pointer_change(diff).unwrap();
        assert_eq!(change.old.unwrap().size, 12_000_000);
        assert_eq!(change.new.unwrap().oid, "sha256:bbbb");

        let added = "diff --git a/A.uasset b/A.uasset\nnew file mode 100644\nindex 0..2\n--- /dev/null\n+++ b/A.uasset\n@@ -0,0 +1,3 @@\n+version https://git-lfs.github.com/spec/v1\n+oid sha256:cccc\n+size 900\n";
        let diff = &parse(added.as_bytes()).unwrap()[0];
        let change = pointer_change(diff).unwrap();
        assert!(change.old.is_none());
        assert_eq!(change.new.unwrap().size, 900);

        let text = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n";
        assert!(pointer_change(&parse(text.as_bytes()).unwrap()[0]).is_none());
    }

    #[test]
    fn progress_lines() {
        assert_eq!(
            describe_progress("download 3/12 1048576/2097152 Content/Maps/Map.umap\n").as_deref(),
            Some("LFS download 3/12 \u{b7} Map.umap 50%")
        );
        assert_eq!(
            describe_progress("upload 1/1 0/0 a.bin").as_deref(),
            Some("LFS upload 1/1 \u{b7} a.bin 100%")
        );
        assert_eq!(describe_progress("garbage"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(12_000_000), "12.0 MB");
        assert_eq!(human_size(14_230_000_000), "14.2 GB");
    }

    #[test]
    fn attributes() {
        let text = b"# Unreal\n*.uasset filter=lfs diff=lfs merge=lfs -text lockable\n*.umap filter=lfs diff=lfs merge=lfs -text\n*.txt text\n";
        assert_eq!(scan_attributes(text), (true, true));
        assert_eq!(
            scan_attributes(b"*.txt text\n# filter=lfs\n"),
            (false, false)
        );
        let out = b"A.uasset\0filter\0lfs\0A.uasset\0lockable\0set\0b.txt\0filter\0unspecified\0b.txt\0lockable\0unspecified\0";
        let attrs = parse_check_attr(out);
        assert_eq!(
            attrs[&BString::from("A.uasset")],
            LfsAttrs {
                lfs: true,
                lockable: true
            }
        );
        assert_eq!(attrs[&BString::from("b.txt")], LfsAttrs::default());
    }

    #[test]
    fn locks_json() {
        let verify = br#"{"ours":[{"id":"1","path":"Content/Map.umap","owner":{"name":"me"},"locked_at":"2026-10-09T10:00:00Z"}],"theirs":[{"id":"2","path":"Content/A.uasset","owner":{"name":"ann"},"locked_at":"2026-10-08T09:00:00Z"}]}"#;
        let locks = parse_locks(verify).unwrap();
        assert_eq!(locks.len(), 2);
        assert!(locks[0].ours && !locks[1].ours);
        assert_eq!(locks[1].owner, "ann");
        let plain = br#"[{"id":"3","path":"x.uasset","owner":{"name":"bob"},"locked_at":""}]"#;
        assert_eq!(parse_locks(plain).unwrap()[0].path, "x.uasset");
        assert!(parse_locks(b"[]").unwrap().is_empty());
        assert!(parse_locks(b"nope").is_err());
    }
}
