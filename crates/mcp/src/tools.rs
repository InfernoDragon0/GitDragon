//! The tools themselves, independent of the MCP plumbing (so tests call them directly).
//!
//! Each returns `Ok(text)` or `Err(message)`, both already redacted and size-capped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bstr::{BString, ByteSlice as _};
use git_core::parse::diff::{FileChange, FileDiff, LineKind};
use git_core::parse::refs::RefKind;
use git_core::parse::status::{EntryKind, FileState};
use git_core::redact::redact;
use git_core::runner::validate_rev;
use git_core::{CliRepo, CommitRequest, DiffTarget, GitCommand, GitRepo as _, GitRunner, LogQuery};
use schemars::JsonSchema;
use serde::Deserialize;

/// Largest tool output, in bytes; longer text is cut with a note.
pub const MAX_OUTPUT: usize = 100_000;
/// Largest error message, in bytes.
const MAX_ERROR: usize = 4_000;
const LOG_DEFAULT: usize = 30;
const LOG_MAX: usize = 500;
const MAX_MESSAGE: usize = 64 * 1024;
const MAX_PATHS: usize = 1_000;

/// The write tools, hidden and refused without `--allow-writes`.
pub const WRITE_TOOLS: [&str; 4] = ["stage", "unstage", "commit", "create_branch"];

pub type ToolResult = Result<String, String>;

/// A repository the server may touch, and the paths that name it.
struct Allowed {
    repo: CliRepo,
    /// Canonical forms of the `--repo` path and of the worktree root.
    keys: Vec<PathBuf>,
}

pub struct GitTools {
    repos: Vec<Allowed>,
    allow_writes: bool,
}

// ---- parameters ------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct RepoParams {
    /// Absolute path of the repository (one passed with --repo). Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct LogParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// Revision or range to start from (`main`, `HEAD~5`, `v1..v2`). Default: HEAD.
    #[serde(default)]
    pub rev: Option<String>,
    /// Repo-relative paths: only commits touching them.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Number of commits (default 30, max 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// Unstaged changes (worktree vs index).
    #[default]
    Worktree,
    /// Staged changes (index vs HEAD).
    Staged,
    /// One commit against its first parent (`rev`).
    Commit,
    /// `from`..`to`.
    Range,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct DiffParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// worktree (default), staged, commit or range.
    #[serde(default)]
    pub target: DiffKind,
    /// The commit, for target=commit.
    #[serde(default)]
    pub rev: Option<String>,
    /// Range start, for target=range.
    #[serde(default)]
    pub from: Option<String>,
    /// Range end, for target=range.
    #[serde(default)]
    pub to: Option<String>,
    /// Repo-relative paths to limit worktree / staged diffs.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BlameParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// Repo-relative file path.
    pub path: String,
    /// Revision to blame at. Default: the working tree.
    #[serde(default)]
    pub rev: Option<String>,
    /// First line (1-based).
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Last line (inclusive).
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BranchesParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// Include remote-tracking branches (default true).
    #[serde(default)]
    pub remote: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ShowParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// The commit (default HEAD).
    #[serde(default)]
    pub rev: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct PathsParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// Repo-relative paths (no absolute paths, no `..`).
    pub paths: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CommitParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// Commit message: summary line, optionally a blank line and a body.
    pub message: String,
    /// Amend HEAD instead of making a new commit (default false).
    #[serde(default)]
    pub amend: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CreateBranchParams {
    /// Absolute path of the repository. Optional when only one is allowed.
    #[serde(default)]
    pub repo: Option<String>,
    /// New branch name.
    pub name: String,
    /// Where it starts (default HEAD).
    #[serde(default)]
    pub start: Option<String>,
    /// Also switch to it (default false).
    #[serde(default)]
    pub switch: bool,
}

// ---- output helpers --------------------------------------------------------------------------

/// Redacts, then caps (in that order: a cut must never leave half a secret unmasked).
pub fn finish(text: &str, max: usize) -> String {
    let mut out = redact(text).into_owned();
    if out.len() > max {
        let mut cut = max;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        let more = out.len() - cut;
        out.truncate(cut);
        let _ = write!(
            out,
            "\n\u{2026} [truncated: {more} more bytes. Narrow the request with paths, a smaller \
             limit or a line range.]"
        );
    }
    out
}

fn fail(err: impl std::fmt::Display) -> String {
    finish(&err.to_string(), MAX_ERROR)
}

fn done(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(text) => Ok(finish(&text, MAX_OUTPUT)),
        Err(err) => Err(fail(err)),
    }
}

fn rev_or_head(rev: Option<&str>) -> Result<String, String> {
    let rev = rev
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("HEAD");
    validate_rev(rev).map_err(fail)?;
    Ok(rev.to_string())
}

fn repo_paths(paths: &[String]) -> Result<Vec<BString>, String> {
    if paths.len() > MAX_PATHS {
        return Err(format!("too many paths (max {MAX_PATHS})"));
    }
    paths
        .iter()
        .map(|p| {
            let p = p.replace('\\', "/");
            git_core::ops::validate_repo_path(p.as_bytes()).map_err(fail)?;
            Ok(BString::from(p))
        })
        .collect()
}

/// `YYYY-MM-DD` of a unix time in a timezone offset.
fn date(secs: i64, tz_minutes: i32) -> String {
    let days = (secs + i64::from(tz_minutes) * 60).div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

fn state_char(state: FileState) -> char {
    match state {
        FileState::Unmodified => '.',
        FileState::Modified => 'M',
        FileState::TypeChanged => 'T',
        FileState::Added => 'A',
        FileState::Deleted => 'D',
        FileState::Renamed => 'R',
        FileState::Copied => 'C',
        FileState::Unmerged => 'U',
    }
}

fn change_char(change: FileChange) -> char {
    match change {
        FileChange::Added => 'A',
        FileChange::Deleted => 'D',
        FileChange::Modified => 'M',
        FileChange::Renamed { .. } => 'R',
        FileChange::Copied { .. } => 'C',
        FileChange::ModeChanged => 'T',
    }
}

/// Unified-diff text of parsed file diffs.
pub fn render_diff(files: &[FileDiff]) -> String {
    let mut out = String::new();
    for file in files {
        out.push_str(&file.header.to_str_lossy());
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if file.binary {
            out.push_str("Binary files differ\n");
        }
        for hunk in &file.hunks {
            out.push_str(&hunk.header.to_str_lossy());
            out.push('\n');
            for line in &hunk.lines {
                let prefix = match line.kind {
                    LineKind::Context => " ",
                    LineKind::Added => "+",
                    LineKind::Deleted => "-",
                    LineKind::NoNewlineAtEof => {
                        out.push_str("\\ No newline at end of file\n");
                        continue;
                    }
                };
                out.push_str(prefix);
                out.push_str(&line.content.to_str_lossy());
                out.push('\n');
            }
        }
        // Stop building once far past the cap; `finish` adds the note.
        if out.len() > MAX_OUTPUT * 2 {
            break;
        }
    }
    if out.is_empty() {
        out.push_str("(no differences)\n");
    }
    out
}

fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

// ---- the tools -------------------------------------------------------------------------------

impl GitTools {
    /// Opens every `--repo` (each must be a git worktree).
    pub async fn open(config: &crate::Config, runner: GitRunner) -> Result<Self, String> {
        let mut repos = Vec::new();
        for path in &config.repos {
            let repo = CliRepo::open(runner.clone(), path).await.map_err(|e| {
                fail(format!(
                    "--repo {}: not a git worktree ({e})",
                    path.display()
                ))
            })?;
            let keys = [path.as_path(), repo.root()]
                .into_iter()
                .filter_map(canonical)
                .collect();
            repos.push(Allowed { repo, keys });
        }
        Ok(Self {
            repos,
            allow_writes: config.allow_writes,
        })
    }

    pub fn allow_writes(&self) -> bool {
        self.allow_writes
    }

    /// The allowed repository `repo` names (the only one, when `repo` is omitted).
    fn pick(&self, repo: Option<&str>) -> Result<&CliRepo, String> {
        let repo = repo.map(str::trim).filter(|r| !r.is_empty());
        let Some(repo) = repo else {
            return match self.repos.as_slice() {
                [only] => Ok(&only.repo),
                _ => Err(format!(
                    "several repositories are allowed; pass `repo` (one of: {})",
                    self.allowed_list()
                )),
            };
        };
        let path = Path::new(repo);
        let found = path
            .is_absolute()
            .then(|| canonical(path))
            .flatten()
            .and_then(|c| self.repos.iter().find(|a| a.keys.contains(&c)));
        found.map(|a| &a.repo).ok_or_else(|| {
            fail(format!(
                "repository not allowed: {repo:?}. Allowed: {}",
                self.allowed_list()
            ))
        })
    }

    fn allowed_list(&self) -> String {
        self.repos
            .iter()
            .map(|a| a.repo.root().display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn check_writes(&self) -> Result<(), String> {
        if self.allow_writes {
            Ok(())
        } else {
            Err("write tools are disabled; the server must be started with --allow-writes".into())
        }
    }

    pub async fn status(&self, p: RepoParams) -> ToolResult {
        done(self.status_inner(p).await)
    }

    async fn status_inner(&self, p: RepoParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let status = repo.status().await.map_err(fail)?;
        let b = &status.branch;
        let mut out = format!(
            "branch: {} ({})\n",
            b.head.as_deref().unwrap_or("(detached)"),
            b.oid.as_deref().map_or("no commits", short),
        );
        if let Some(up) = &b.upstream {
            let _ = writeln!(
                out,
                "upstream: {up} (ahead {}, behind {})",
                b.ahead, b.behind
            );
        }
        if status.stash_count > 0 {
            let _ = writeln!(out, "stashes: {}", status.stash_count);
        }
        if status.entries.is_empty() {
            out.push_str("working tree clean\n");
        } else {
            out.push_str("changes (index, worktree, path):\n");
        }
        for e in &status.entries {
            let code = match e.kind {
                EntryKind::Untracked => "??".to_string(),
                EntryKind::Ignored => "!!".to_string(),
                _ => format!("{}{}", state_char(e.index), state_char(e.worktree)),
            };
            let conflict = if e.kind == EntryKind::Unmerged {
                " (conflict)"
            } else {
                ""
            };
            let _ = write!(out, "  {code} {}", e.path.to_str_lossy());
            if let Some(orig) = &e.orig_path {
                let _ = write!(out, " (from {})", orig.to_str_lossy());
            }
            let _ = writeln!(out, "{conflict}");
        }
        Ok(out)
    }

    pub async fn log(&self, p: LogParams) -> ToolResult {
        done(self.log_inner(p).await)
    }

    async fn log_inner(&self, p: LogParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let rev = rev_or_head(p.rev.as_deref())?;
        let query = LogQuery {
            revs: vec![rev],
            topo_order: false,
            limit: Some(p.limit.unwrap_or(LOG_DEFAULT).clamp(1, LOG_MAX)),
            paths: repo_paths(&p.paths)?,
            bodies: false,
            ..LogQuery::default()
        };
        let commits = repo.log(query).await.map_err(fail)?;
        let mut out = String::new();
        for c in &commits {
            let _ = writeln!(
                out,
                "{} {} {} | {}",
                short(&c.id),
                date(c.author.time, c.author.tz_offset_minutes),
                c.author.name.to_str_lossy(),
                c.subject.to_str_lossy()
            );
        }
        if out.is_empty() {
            out.push_str("(no commits)\n");
        }
        Ok(out)
    }

    pub async fn diff(&self, p: DiffParams) -> ToolResult {
        done(self.diff_inner(p).await)
    }

    async fn diff_inner(&self, p: DiffParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let paths = repo_paths(&p.paths)?;
        let need = |v: &Option<String>, what: &str| -> Result<String, String> {
            let v = v.as_deref().map(str::trim).filter(|v| !v.is_empty());
            let v = v.ok_or_else(|| format!("target needs `{what}`"))?;
            validate_rev(v).map_err(fail)?;
            Ok(v.to_string())
        };
        let target = match p.target {
            DiffKind::Worktree => DiffTarget::Unstaged { paths },
            DiffKind::Staged => DiffTarget::Staged { paths },
            DiffKind::Commit | DiffKind::Range if !paths.is_empty() => {
                return Err("`paths` only applies to worktree and staged diffs".into());
            }
            DiffKind::Commit => DiffTarget::Commit {
                id: need(&p.rev, "rev")?,
            },
            DiffKind::Range => DiffTarget::Range {
                from: need(&p.from, "from")?,
                to: need(&p.to, "to")?,
            },
        };
        let files = repo.diff(target).await.map_err(fail)?;
        Ok(render_diff(&files))
    }

    pub async fn blame(&self, p: BlameParams) -> ToolResult {
        done(self.blame_inner(p).await)
    }

    async fn blame_inner(&self, p: BlameParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let path = repo_paths(std::slice::from_ref(&p.path))?;
        let mut cmd = GitCommand::read(repo.root()).args(["blame", "--date=short"]);
        let range = match (p.start_line, p.end_line) {
            (None, None) => None,
            (Some(0), _) | (_, Some(0)) => return Err("line numbers start at 1".into()),
            (Some(s), Some(e)) if e < s => return Err("end_line is before start_line".into()),
            (Some(s), Some(e)) => Some(format!("{s},{e}")),
            (Some(s), None) => Some(format!("{s},")),
            (None, Some(e)) => Some(format!("1,{e}")),
        };
        if let Some(range) = range {
            cmd = cmd.arg("-L").arg(range);
        }
        if let Some(rev) = p.rev.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            cmd = cmd.rev(rev).map_err(fail)?;
        }
        let out = repo.runner().run(cmd.paths(&path)).await.map_err(fail)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub async fn branches(&self, p: BranchesParams) -> ToolResult {
        done(self.branches_inner(p).await)
    }

    async fn branches_inner(&self, p: BranchesParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let refs = repo.refs().await.map_err(fail)?;
        let mut out = String::from("local:\n");
        for r in refs.iter().filter(|r| r.kind == RefKind::LocalBranch) {
            let mark = if r.is_head { '*' } else { ' ' };
            let _ = write!(out, "{mark} {} {}", r.short_name, short(&r.oid));
            if let Some(up) = &r.upstream {
                if up.gone {
                    let _ = write!(out, " -> {} [gone]", up.name);
                } else {
                    let _ = write!(
                        out,
                        " -> {} [ahead {}, behind {}]",
                        up.name, up.ahead, up.behind
                    );
                }
            }
            let _ = writeln!(out, " | {}", r.subject.to_str_lossy());
        }
        if p.remote.unwrap_or(true) {
            out.push_str("remote:\n");
            for r in refs.iter().filter(|r| r.kind == RefKind::RemoteBranch) {
                let _ = writeln!(out, "  {} {}", r.short_name, short(&r.oid));
            }
        }
        Ok(out)
    }

    pub async fn show(&self, p: ShowParams) -> ToolResult {
        done(self.show_inner(p).await)
    }

    async fn show_inner(&self, p: ShowParams) -> Result<String, String> {
        let repo = self.pick(p.repo.as_deref())?;
        let rev = rev_or_head(p.rev.as_deref())?;
        let query = LogQuery {
            revs: vec![rev.clone()],
            topo_order: false,
            limit: Some(1),
            ..LogQuery::default()
        };
        let commit = repo
            .log(query)
            .await
            .map_err(fail)?
            .into_iter()
            .next()
            .ok_or_else(|| format!("no commit at {rev}"))?;
        let files = repo.commit_files(&commit.id, true).await.map_err(fail)?;
        let sig = |s: &git_core::parse::log::Signature| {
            format!(
                "{} <{}> {}",
                s.name.to_str_lossy(),
                s.email.to_str_lossy(),
                date(s.time, s.tz_offset_minutes)
            )
        };
        let mut out = format!("commit {}\n", commit.id);
        if !commit.parents.is_empty() {
            let _ = writeln!(out, "parents: {}", commit.parents.join(" "));
        }
        let _ = writeln!(out, "author: {}", sig(&commit.author));
        let _ = writeln!(out, "committer: {}", sig(&commit.committer));
        let _ = writeln!(out, "\n    {}", commit.subject.to_str_lossy());
        let body = commit.body.to_str_lossy();
        if !body.trim().is_empty() {
            out.push('\n');
            for line in body.trim_end().lines() {
                let _ = writeln!(out, "    {line}");
            }
        }
        let _ = writeln!(out, "\nfiles ({}):", files.len());
        for f in &files {
            let counts = match (f.added, f.removed) {
                (Some(a), Some(r)) => format!("+{a} -{r}"),
                _ => "binary".into(),
            };
            let _ = write!(
                out,
                "  {} {counts} {}",
                change_char(f.change),
                f.path().to_str_lossy()
            );
            if let (FileChange::Renamed { .. } | FileChange::Copied { .. }, Some(old)) =
                (f.change, &f.old_path)
            {
                let _ = write!(out, " (from {})", old.to_str_lossy());
            }
            out.push('\n');
        }
        Ok(out)
    }

    pub async fn stage(&self, p: PathsParams) -> ToolResult {
        done(self.stage_inner(p, true).await)
    }

    pub async fn unstage(&self, p: PathsParams) -> ToolResult {
        done(self.stage_inner(p, false).await)
    }

    async fn stage_inner(&self, p: PathsParams, stage: bool) -> Result<String, String> {
        self.check_writes()?;
        let repo = self.pick(p.repo.as_deref())?;
        let paths = repo_paths(&p.paths)?;
        if paths.is_empty() {
            return Err("`paths` is empty".into());
        }
        if stage {
            repo.stage(&paths).await.map_err(fail)?;
        } else {
            repo.unstage(&paths).await.map_err(fail)?;
        }
        let verb = if stage { "staged" } else { "unstaged" };
        Ok(format!("{verb} {} path(s)", paths.len()))
    }

    pub async fn commit(&self, p: CommitParams) -> ToolResult {
        done(self.commit_inner(p).await)
    }

    async fn commit_inner(&self, p: CommitParams) -> Result<String, String> {
        self.check_writes()?;
        let repo = self.pick(p.repo.as_deref())?;
        if p.message.len() > MAX_MESSAGE {
            return Err(format!("message is too long (max {MAX_MESSAGE} bytes)"));
        }
        let message = p.message.trim();
        let (summary, body) = message.split_once('\n').unwrap_or((message, ""));
        if summary.trim().is_empty() {
            return Err("the message needs a summary line".into());
        }
        let req = CommitRequest {
            summary: summary.trim().to_string(),
            body: body.trim().to_string(),
            amend: p.amend,
            ..CommitRequest::default()
        };
        let id = repo.commit(&req).await.map_err(fail)?;
        let verb = if p.amend { "amended" } else { "committed" };
        Ok(format!("{verb} {id}"))
    }

    pub async fn create_branch(&self, p: CreateBranchParams) -> ToolResult {
        done(self.create_branch_inner(p).await)
    }

    async fn create_branch_inner(&self, p: CreateBranchParams) -> Result<String, String> {
        self.check_writes()?;
        let repo = self.pick(p.repo.as_deref())?;
        let start = rev_or_head(p.start.as_deref())?;
        let name = repo.check_branch_name(&p.name).await.map_err(fail)?;
        repo.create_branch(&name, &start, p.switch)
            .await
            .map_err(fail)?;
        let switched = if p.switch { " and switched to it" } else { "" };
        Ok(format!("created branch {name} at {start}{switched}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0, 0), "1970-01-01");
        assert_eq!(date(951_782_400, 0), "2000-02-29");
        assert_eq!(date(1_700_000_000, 0), "2023-11-14");
        assert_eq!(date(1_700_000_000, -24 * 60), "2023-11-13");
    }

    #[test]
    fn finish_caps_on_a_char_boundary() {
        let text = "\u{e9}".repeat(10);
        let out = finish(&text, 5);
        assert!(out.starts_with("\u{e9}\u{e9}"));
        assert!(out.contains("truncated: 16 more bytes"));
        assert_eq!(finish("short", 100), "short");
    }

    #[test]
    fn paths_are_validated() {
        assert!(repo_paths(&["src/lib.rs".into(), "a\\b.txt".into()]).is_ok());
        for bad in ["/etc/passwd", "../x", "a/../../x", ""] {
            assert!(repo_paths(&[bad.into()]).is_err(), "{bad}");
        }
    }
}
