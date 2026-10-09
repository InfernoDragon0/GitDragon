//! Async git runner: argv-only spawning with hardened env and flags, timeouts, cancellation
//! (drop the future: the child is killed), streamed stdout, and a command log for the UI's
//! operation strip.
//!
//! Security (checklist P0):
//! - argv only, never `sh -c` / `cmd /c`; user refs go through [`GitCommand::rev`] (rejects
//!   anything that could read as an option) and paths after `--` with literal pathspecs.
//! - git runs from the absolute path found by [`crate::detect`]; `PATH` handed to git keeps
//!   only absolute entries, and on Windows child processes never search the current dir.
//! - Background reads ([`Mode::Read`]) run with `--no-optional-locks`, `core.fsmonitor=false`,
//!   hooks disabled and no terminal prompts.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_process::{Child, ChildStdout};
use bstr::ByteSlice;
use futures_lite::{AsyncReadExt as _, AsyncWriteExt as _, future};

use crate::detect::{self, GitBinary};
use crate::error::{FailureKind, GitError, Result, stderr_tail};
use crate::progress::{self, LineSplitter, Progress};

/// Default timeout for [`Mode::Read`] commands.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of stderr kept per command (the tail matters, not the whole stream).
const STDERR_CAP: usize = 64 * 1024;
/// Records kept in the [`CommandLog`].
const LOG_CAP: usize = 200;
/// Chunk size for streamed stdout.
const CHUNK: usize = 64 * 1024;

/// Global options on every command.
const BASE_FLAGS: &[&str] = &[
    "-c",
    "core.quotepath=off",
    "-c",
    "color.ui=never",
    "-c",
    "core.pager=cat",
    "-c",
    "log.showSignature=false",
];

/// Extra global options for background reads.
fn read_flags() -> [&'static str; 6] {
    // A hooks dir that cannot exist, so no hook (e.g. post-index-change) can run.
    let no_hooks = if cfg!(windows) {
        "core.hooksPath=NUL"
    } else {
        "core.hooksPath=/dev/null"
    };
    [
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        no_hooks,
        "--no-replace-objects",
    ]
}

/// Inherited variables that would redirect git away from the repo we chose, or inject config.
const ENV_REMOVE: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_EXTERNAL_DIFF",
    "GIT_EXEC_PATH",
];

/// Applies the env / window hardening shared by every git spawn (also `git --version`).
pub(crate) fn harden_std_command(cmd: &mut std::process::Command) {
    for var in ENV_REMOVE {
        cmd.env_remove(var);
    }
    cmd.env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Ok(path) = std::env::join_paths(detect::absolute_path_dirs(std::env::var_os("PATH"))) {
        cmd.env("PATH", path);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        // CreateProcess / SearchPath skip the current dir, so a repo can't plant ssh.exe etc.
        cmd.env("NoDefaultCurrentDirectoryInExePath", "1");
    }
}

/// Read-only background work vs user-initiated operations (which keep hooks, fsmonitor and
/// the user's config exactly as in a terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

/// One git invocation. Build with [`GitCommand::read`] / [`GitCommand::write`].
#[derive(Debug, Clone)]
pub struct GitCommand {
    cwd: PathBuf,
    mode: Mode,
    args: Vec<OsString>,
    literal_pathspecs: bool,
    timeout: Option<Duration>,
    stdin: Option<Vec<u8>>,
}

impl GitCommand {
    pub fn read(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd.into(), Mode::Read, Some(READ_TIMEOUT))
    }

    /// No timeout by default: fetch / push / rebase may legitimately take long.
    pub fn write(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd.into(), Mode::Write, None)
    }

    fn new(cwd: PathBuf, mode: Mode, timeout: Option<Duration>) -> Self {
        Self {
            cwd,
            mode,
            args: Vec::new(),
            literal_pathspecs: false,
            timeout,
            stdin: None,
        }
    }

    /// A fixed argument chosen by GitDragon (subcommand, flag). Never pass user input here:
    /// use [`Self::rev`] or [`Self::paths`].
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// A user-supplied revision / ref / range (`main`, `origin/x`, `a...b`, `HEAD~2`).
    pub fn rev(mut self, rev: &str) -> Result<Self> {
        validate_rev(rev)?;
        self.args.push(rev.into());
        Ok(self)
    }

    /// Other user input placed after `--` (a URL, a folder): may contain spaces, but never
    /// starts with `-` or contains control characters.
    pub fn user_arg(mut self, what: &'static str, value: impl AsRef<OsStr>) -> Result<Self> {
        let value = value.as_ref();
        let text = value.to_string_lossy();
        if text.is_empty() || text.starts_with('-') || text.chars().any(char::is_control) {
            return Err(GitError::InvalidArgument {
                what,
                value: text.into_owned(),
            });
        }
        self.args.push(value.to_owned());
        Ok(self)
    }

    /// Appends `--` and the paths, matched literally (no glob or `:(magic)` pathspecs).
    pub fn paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<[u8]>,
    {
        self.args.push("--".into());
        self.args
            .extend(paths.into_iter().map(|p| bytes_to_os(p.as_ref())));
        self.literal_pathspecs = true;
        self
    }

    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn stdin(mut self, input: Vec<u8>) -> Self {
        self.stdin = Some(input);
        self
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `git status --porcelain=v2 ...` as shown in the operation strip (safety flags omitted).
    pub fn display(&self) -> String {
        let mut out = String::from("git");
        for arg in &self.args {
            let arg = arg.to_string_lossy();
            out.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                out.push('"');
                out.push_str(&arg);
                out.push('"');
            } else {
                out.push_str(&arg);
            }
        }
        out
    }

    fn to_command(&self, git: &GitBinary) -> async_process::Command {
        let mut cmd = std::process::Command::new(&git.path);
        cmd.args(BASE_FLAGS);
        if self.mode == Mode::Read {
            cmd.args(read_flags());
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
        }
        cmd.args(&self.args).current_dir(&self.cwd);
        harden_std_command(&mut cmd);
        if self.literal_pathspecs {
            cmd.env("GIT_LITERAL_PATHSPECS", "1");
        }
        // Stdio must be set after the conversion: `From<std::process::Command>` drops it.
        let mut cmd = async_process::Command::from(cmd);
        cmd.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        cmd
    }
}

/// Rejects revisions that git could read as an option, or that can't be a revision at all.
pub fn validate_rev(rev: &str) -> Result<()> {
    let bad = rev.is_empty()
        || rev.starts_with('-')
        || rev.len() > 4096
        || rev.chars().any(|c| c.is_control() || c == ' ');
    if bad {
        return Err(GitError::InvalidArgument {
            what: "revision",
            value: rev.to_string(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(bytes.to_vec())
}

/// Git for Windows speaks UTF-8 paths.
#[cfg(not(unix))]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    OsString::from(bytes.to_str_lossy().into_owned())
}

/// Finished command.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs git commands for any number of repos; cheap to clone.
#[derive(Clone)]
pub struct GitRunner {
    git: Arc<GitBinary>,
    log: CommandLog,
}

impl GitRunner {
    pub fn new(git: GitBinary) -> Self {
        Self {
            git: Arc::new(git),
            log: CommandLog::default(),
        }
    }

    /// Uses the cached [`detect::git`] result (blocking on first call).
    pub fn detect() -> Result<Self> {
        Ok(Self::new(detect::git()?))
    }

    pub fn git(&self) -> &GitBinary {
        &self.git
    }

    pub fn log(&self) -> &CommandLog {
        &self.log
    }

    /// Runs to completion; a non-zero exit is an error. Returns stdout.
    pub async fn run(&self, cmd: GitCommand) -> Result<Vec<u8>> {
        let display = cmd.display();
        let out = self.output(cmd).await?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(failed(display, out.code, &out.stderr))
        }
    }

    /// Runs to completion and returns the exit code instead of failing on it (for commands
    /// where a non-zero exit is an answer, e.g. `merge-base --is-ancestor`).
    pub async fn output(&self, cmd: GitCommand) -> Result<GitOutput> {
        let display = cmd.display();
        let mut guard = self.log.start(&cmd);
        let mut child = self.spawn(&cmd)?;

        let stdin = child.stdin.take();
        let input = cmd.stdin.clone();
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let work = async move {
            let write_stdin = async {
                if let (Some(mut pipe), Some(input)) = (stdin, input) {
                    let written = async {
                        pipe.write_all(&input).await?;
                        pipe.close().await
                    };
                    match written.await {
                        // git stopped reading (usually it failed): its exit code and stderr
                        // explain why, so report those instead of the pipe error.
                        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                        other => other?,
                    }
                }
                Ok::<_, std::io::Error>(())
            };
            let read_stdout = async {
                let mut buf = Vec::new();
                if let Some(pipe) = stdout_pipe.as_mut() {
                    pipe.read_to_end(&mut buf).await?;
                }
                Ok::<_, std::io::Error>(buf)
            };
            let read_stderr = async {
                match stderr_pipe.as_mut() {
                    Some(pipe) => read_capped(pipe, STDERR_CAP).await,
                    None => Ok(Vec::new()),
                }
            };
            let ((w, out), err) =
                future::zip(future::zip(write_stdin, read_stdout), read_stderr).await;
            w?;
            let status = child.status().await?;
            Ok::<_, std::io::Error>(GitOutput {
                code: status.code(),
                stdout: out?,
                stderr: err?,
            })
        };

        let result = match cmd.timeout {
            Some(after) => {
                let timed_out = async {
                    async_io::Timer::after(after).await;
                    Err(GitError::Timeout {
                        command: display.clone(),
                        after,
                    })
                };
                future::or(async { work.await.map_err(GitError::Io) }, timed_out).await
            }
            None => work.await.map_err(GitError::Io),
        };
        guard.finish(match &result {
            Ok(out) if out.success() => Outcome::Ok,
            Ok(out) => Outcome::Failed { code: out.code },
            Err(GitError::Timeout { .. }) => Outcome::TimedOut,
            Err(_) => Outcome::Failed { code: None },
        });
        result
    }

    /// Runs to completion, reporting `--progress` lines from stderr as they arrive (clone,
    /// fetch, push). A non-zero exit is an error. Drop the future to cancel.
    pub async fn run_with_progress(
        &self,
        cmd: GitCommand,
        mut on_progress: impl FnMut(Progress) + Send,
    ) -> Result<Vec<u8>> {
        let display = cmd.display();
        let mut guard = self.log.start(&cmd);
        let mut child = self.spawn(&cmd)?;
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let read_stdout = async {
            let mut buf = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                pipe.read_to_end(&mut buf).await?;
            }
            Ok::<_, std::io::Error>(buf)
        };
        let read_stderr = async {
            let mut tail = VecDeque::new();
            let mut lines = LineSplitter::default();
            let mut buf = [0u8; 4096];
            let mut emit = |line: &str| {
                if let Some(progress) = progress::parse_line(line) {
                    on_progress(progress);
                }
            };
            if let Some(pipe) = stderr_pipe.as_mut() {
                loop {
                    let n = pipe.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    lines.push(&buf[..n], &mut emit);
                    tail.extend(&buf[..n]);
                    let excess = tail.len().saturating_sub(STDERR_CAP);
                    tail.drain(..excess);
                }
            }
            lines.finish(&mut emit);
            Ok::<_, std::io::Error>(Vec::from(tail))
        };
        let (out, err) = future::zip(read_stdout, read_stderr).await;
        let (out, err) = (out?, err?);
        let status = child.status().await?;
        if status.success() {
            guard.finish(Outcome::Ok);
            Ok(out)
        } else {
            guard.finish(Outcome::Failed {
                code: status.code(),
            });
            Err(failed(display, status.code(), &err))
        }
    }

    /// Spawns and streams stdout (e.g. `git log` for the graph). No timeout; drop the stream
    /// to cancel. Call [`GitStream::finish`] after the last chunk to surface a failure.
    pub fn stream(&self, cmd: GitCommand) -> Result<GitStream> {
        let guard = self.log.start(&cmd);
        let mut child = self.spawn(&cmd)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take().map(|mut pipe| {
            // Drain stderr on its own thread so a chatty git can't block on a full pipe.
            std::thread::spawn(move || {
                async_io::block_on(read_capped(&mut pipe, STDERR_CAP)).unwrap_or_default()
            })
        });
        Ok(GitStream {
            display: cmd.display(),
            child,
            stdout,
            stderr,
            guard,
        })
    }

    fn spawn(&self, cmd: &GitCommand) -> Result<Child> {
        tracing::debug!(command = %cmd.display(), cwd = %cmd.cwd.display(), "git");
        cmd.to_command(&self.git)
            .spawn()
            .map_err(|source| GitError::Spawn {
                program: self.git.path.clone(),
                source,
            })
    }
}

/// A running command whose stdout is read in chunks.
pub struct GitStream {
    display: String,
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
    guard: LogGuard,
}

impl GitStream {
    /// Next stdout chunk (up to 64 KiB), or `None` at EOF.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(pipe) = self.stdout.as_mut() else {
            return Ok(None);
        };
        let mut buf = vec![0; CHUNK];
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            self.stdout = None;
            return Ok(None);
        }
        buf.truncate(n);
        Ok(Some(buf))
    }

    /// Waits for exit; a non-zero exit becomes [`GitError::Failed`].
    pub async fn finish(mut self) -> Result<()> {
        self.stdout = None;
        let status = self.child.status().await?;
        let stderr = self
            .stderr
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        if status.success() {
            self.guard.finish(Outcome::Ok);
            Ok(())
        } else {
            self.guard.finish(Outcome::Failed {
                code: status.code(),
            });
            Err(failed(self.display.clone(), status.code(), &stderr))
        }
    }
}

async fn read_capped<R: futures_lite::AsyncRead + Unpin>(
    pipe: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let mut kept = VecDeque::with_capacity(4096);
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        kept.extend(&buf[..n]);
        let excess = kept.len().saturating_sub(cap);
        kept.drain(..excess);
    }
    Ok(kept.into())
}

fn failed(command: String, code: Option<i32>, stderr: &[u8]) -> GitError {
    let tail = stderr_tail(stderr, 8);
    GitError::Failed {
        command,
        code,
        kind: FailureKind::classify(&tail),
        stderr_tail: tail,
    }
}

// ---- command log --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Running,
    Ok,
    Failed {
        code: Option<i32>,
    },
    TimedOut,
    /// The future / stream was dropped before git finished (the child was killed).
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct CommandRecord {
    pub id: u64,
    /// As shown to the user: `git status --porcelain=v2 ...`.
    pub command: String,
    pub cwd: PathBuf,
    pub mode: Mode,
    pub started: SystemTime,
    pub duration: Option<Duration>,
    pub outcome: Outcome,
}

type Listener = Arc<dyn Fn(&CommandRecord) + Send + Sync>;

/// The most recent commands with their outcome, for the operation strip.
#[derive(Clone, Default)]
pub struct CommandLog {
    inner: Arc<Mutex<LogInner>>,
}

#[derive(Default)]
struct LogInner {
    next_id: u64,
    records: VecDeque<CommandRecord>,
    listener: Option<Listener>,
}

impl CommandLog {
    /// Newest last.
    pub fn records(&self) -> Vec<CommandRecord> {
        self.lock().records.iter().cloned().collect()
    }

    /// Called on every start and finish (from whichever thread ran the command).
    pub fn set_listener(&self, listener: impl Fn(&CommandRecord) + Send + Sync + 'static) {
        self.lock().listener = Some(Arc::new(listener));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start(&self, cmd: &GitCommand) -> LogGuard {
        let (record, listener) = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let record = CommandRecord {
                id: inner.next_id,
                command: cmd.display(),
                cwd: cmd.cwd.clone(),
                mode: cmd.mode,
                started: SystemTime::now(),
                duration: None,
                outcome: Outcome::Running,
            };
            inner.records.push_back(record.clone());
            if inner.records.len() > LOG_CAP {
                inner.records.pop_front();
            }
            (record, inner.listener.clone())
        };
        if let Some(listener) = listener {
            listener(&record);
        }
        LogGuard {
            log: self.clone(),
            id: record.id,
            started: Instant::now(),
            done: false,
        }
    }

    fn finish(&self, id: u64, duration: Duration, outcome: Outcome) {
        let (record, listener) = {
            let mut inner = self.lock();
            let Some(record) = inner.records.iter_mut().find(|r| r.id == id) else {
                return;
            };
            record.duration = Some(duration);
            record.outcome = outcome;
            let record = record.clone();
            (record, inner.listener.clone())
        };
        if let Some(listener) = listener {
            listener(&record);
        }
    }
}

/// Marks its record finished; dropped unfinished means the command was cancelled.
struct LogGuard {
    log: CommandLog,
    id: u64,
    started: Instant,
    done: bool,
}

impl LogGuard {
    fn finish(&mut self, outcome: Outcome) {
        self.done = true;
        self.log.finish(self.id, self.started.elapsed(), outcome);
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if !self.done {
            self.log
                .finish(self.id, self.started.elapsed(), Outcome::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> GitRunner {
        GitRunner::detect().expect("git is installed")
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = runner();
        async_io::block_on(r.run(GitCommand::write(dir.path()).args(["init", "-q"]))).unwrap();
        dir
    }

    #[test]
    fn rejects_option_like_revisions() {
        for bad in ["", "-x", "--upload-pack=evil", "a b", "a\nb", "x\0y"] {
            assert!(validate_rev(bad).is_err(), "{bad:?}");
        }
        for ok in [
            "main",
            "origin/main",
            "HEAD~2",
            "a...b",
            "v1.0^{commit}",
            "refs/heads/x",
        ] {
            assert!(validate_rev(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn display_hides_safety_flags() {
        let cmd = GitCommand::read(".")
            .args(["log", "--format=%H %s"])
            .paths(["a b.txt"]);
        assert_eq!(cmd.display(), r#"git log "--format=%H %s" -- "a b.txt""#);
    }

    #[test]
    fn runs_and_logs() {
        let dir = repo();
        let r = runner();
        let out = async_io::block_on(
            r.run(GitCommand::read(dir.path()).args(["rev-parse", "--is-inside-work-tree"])),
        )
        .unwrap();
        assert_eq!(out.trim_ascii(), b"true");
        let records = r.log().records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Outcome::Ok);
        assert!(records[0].duration.is_some());
    }

    #[test]
    fn failure_is_structured() {
        let dir = tempfile::tempdir().unwrap();
        let err = async_io::block_on(
            runner().run(GitCommand::read(dir.path()).args(["rev-parse", "HEAD"])),
        )
        .unwrap_err();
        match err {
            GitError::Failed { kind, code, .. } => {
                assert_eq!(kind, FailureKind::NotARepository);
                assert_ne!(code, Some(0));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stdin_and_literal_paths() {
        let dir = repo();
        let r = runner();
        // hash-object reads stdin; `*` must be taken literally, not as a glob.
        let oid = async_io::block_on(
            r.run(
                GitCommand::read(dir.path())
                    .args(["hash-object", "--stdin"])
                    .stdin(b"hello\n".to_vec()),
            ),
        )
        .unwrap();
        assert_eq!(
            oid.trim_ascii(),
            b"ce013625030ba8dba906f756967f9e9ca394464a"
        );

        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let listed = async_io::block_on(
            r.run(
                GitCommand::read(dir.path())
                    .args(["ls-files", "--others"])
                    .paths(["*.txt"]),
            ),
        )
        .unwrap();
        assert!(listed.is_empty(), "glob matched: {:?}", listed.as_bstr());
    }

    #[test]
    fn timeout_kills() {
        let dir = repo();
        let r = runner();
        // A zero timeout fires before a freshly spawned git can possibly have exited.
        let result = async_io::block_on(
            r.output(
                GitCommand::read(dir.path())
                    .args(["log", "--all"])
                    .timeout(Some(Duration::ZERO)),
            ),
        );
        assert!(
            matches!(result, Err(GitError::Timeout { .. })),
            "{result:?}"
        );
        assert_eq!(r.log().records()[0].outcome, Outcome::TimedOut);
    }

    #[test]
    fn streams_and_cancels() {
        let dir = repo();
        let r = runner();
        for i in 0..3 {
            let cmd = GitCommand::write(dir.path()).args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
            ]);
            async_io::block_on(r.run(cmd.arg(format!("c{i}")))).unwrap();
        }
        let mut stream = r
            .stream(GitCommand::read(dir.path()).args(["log", "--format=%s"]))
            .unwrap();
        let mut all = Vec::new();
        while let Some(chunk) = async_io::block_on(stream.next_chunk()).unwrap() {
            all.extend(chunk);
        }
        async_io::block_on(stream.finish()).unwrap();
        assert_eq!(all, b"c2\nc1\nc0\n");

        let stream = r
            .stream(GitCommand::read(dir.path()).args(["log", "--format=%s"]))
            .unwrap();
        drop(stream);
        let last = r.log().records().pop().unwrap();
        assert_eq!(last.outcome, Outcome::Cancelled);
    }
}
