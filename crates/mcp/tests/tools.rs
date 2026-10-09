//! End-to-end tool calls against temp repositories: validation, repo allow-list, write gating.

use std::path::Path;
use std::process::Command;

use mcp::tools::{
    BlameParams, BranchesParams, CommitParams, CreateBranchParams, DiffKind, DiffParams, LogParams,
    PathsParams, RepoParams, ShowParams,
};
use mcp::{Config, GitServer, GitTools};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// A repo with two commits on `main` and identity in its config.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.name", "Test"]);
    git(d, &["config", "user.email", "test@example.com"]);
    git(d, &["config", "commit.gpgsign", "false"]);
    std::fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git(d, &["add", "a.txt"]);
    git(d, &["commit", "-q", "-m", "first commit"]);
    std::fs::write(d.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    git(d, &["commit", "-q", "-am", "second commit\n\nbody text"]);
    dir
}

fn abs(dir: &Path) -> String {
    dir.display().to_string()
}

async fn tools(dirs: &[&Path], allow_writes: bool) -> GitTools {
    let config = Config {
        repos: dirs.iter().map(|d| d.to_path_buf()).collect(),
        allow_writes,
    };
    let runner = git_core::GitRunner::detect().expect("git is installed");
    GitTools::open(&config, runner).await.expect("repos open")
}

#[tokio::test]
async fn read_tools_answer() {
    let dir = fixture();
    let d = dir.path();
    let t = tools(&[d], false).await;
    std::fs::write(d.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(d.join("new.txt"), "x").unwrap();

    let status = t.status(RepoParams::default()).await.unwrap();
    assert!(status.contains("branch: main"), "{status}");
    assert!(
        status.contains(".M a.txt") && status.contains("?? new.txt"),
        "{status}"
    );

    let log = t.log(LogParams::default()).await.unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(
        log.lines().next().unwrap().ends_with("| second commit"),
        "{log}"
    );
    let one = t
        .log(LogParams {
            limit: Some(1),
            paths: vec!["a.txt".into()],
            rev: Some("HEAD~1".into()),
            ..LogParams::default()
        })
        .await
        .unwrap();
    assert!(
        one.contains("first commit") && one.lines().count() == 1,
        "{one}"
    );

    let diff = t.diff(DiffParams::default()).await.unwrap();
    assert!(diff.contains("+four"), "{diff}");
    let commit_diff = t
        .diff(DiffParams {
            target: DiffKind::Commit,
            rev: Some("HEAD".into()),
            ..DiffParams::default()
        })
        .await
        .unwrap();
    assert!(
        commit_diff.contains("-two") && commit_diff.contains("+TWO"),
        "{commit_diff}"
    );
    let range = t
        .diff(DiffParams {
            target: DiffKind::Range,
            from: Some("HEAD~1".into()),
            to: Some("HEAD".into()),
            ..DiffParams::default()
        })
        .await
        .unwrap();
    assert!(range.contains("+TWO"), "{range}");

    let blame = t
        .blame(BlameParams {
            path: "a.txt".into(),
            rev: Some("HEAD".into()),
            start_line: Some(2),
            end_line: Some(2),
            ..BlameParams::default()
        })
        .await
        .unwrap();
    assert_eq!(blame.lines().count(), 1, "{blame}");
    assert!(blame.contains("TWO"), "{blame}");

    git(d, &["branch", "topic"]);
    let branches = t.branches(BranchesParams::default()).await.unwrap();
    assert!(
        branches.contains("* main") && branches.contains("  topic"),
        "{branches}"
    );

    let show = t.show(ShowParams::default()).await.unwrap();
    assert!(
        show.contains("second commit") && show.contains("body text"),
        "{show}"
    );
    assert!(show.contains("M +1 -1 a.txt"), "{show}");
}

#[tokio::test]
async fn params_are_validated() {
    let dir = fixture();
    let t = tools(&[dir.path()], false).await;
    let bad_rev = t
        .log(LogParams {
            rev: Some("--output=/tmp/x".into()),
            ..LogParams::default()
        })
        .await;
    assert!(bad_rev.unwrap_err().contains("invalid revision"));
    let bad_path = t
        .diff(DiffParams {
            paths: vec!["../outside".into()],
            ..DiffParams::default()
        })
        .await;
    assert!(bad_path.unwrap_err().contains("invalid path"));
    let abs_path = t
        .blame(BlameParams {
            path: abs(dir.path()),
            ..BlameParams::default()
        })
        .await;
    assert!(abs_path.is_err());
    let lines = t
        .blame(BlameParams {
            path: "a.txt".into(),
            start_line: Some(3),
            end_line: Some(1),
            ..BlameParams::default()
        })
        .await;
    assert!(lines.is_err());
    let missing = t
        .diff(DiffParams {
            target: DiffKind::Commit,
            ..DiffParams::default()
        })
        .await;
    assert!(missing.unwrap_err().contains("rev"));
}

#[tokio::test]
async fn only_allowed_repos_are_reachable() {
    let (one, two, other) = (fixture(), fixture(), fixture());
    let t = tools(&[one.path(), two.path()], false).await;
    // Ambiguous without `repo`.
    assert!(t.status(RepoParams::default()).await.is_err());
    let picked = t
        .status(RepoParams {
            repo: Some(abs(two.path())),
        })
        .await;
    assert!(picked.is_ok(), "{picked:?}");
    for repo in [
        abs(other.path()),
        "relative".into(),
        abs(&two.path().join("..")),
    ] {
        let err = t.status(RepoParams { repo: Some(repo) }).await.unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
    }
}

#[tokio::test]
async fn writes_are_refused_without_the_flag() {
    let dir = fixture();
    let d = dir.path();
    std::fs::write(d.join("b.txt"), "b").unwrap();
    let head = git(d, &["rev-parse", "HEAD"]);

    let ro = tools(&[d], false).await;
    let names = GitServer::new(tools(&[d], false).await).tool_names();
    for write in mcp::tools::WRITE_TOOLS {
        assert!(
            !names.iter().any(|n| n == write),
            "{write} listed: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == "status") && names.len() == 6,
        "{names:?}"
    );
    let paths = || PathsParams {
        repo: None,
        paths: vec!["b.txt".into()],
    };
    for result in [
        ro.stage(paths()).await,
        ro.unstage(paths()).await,
        ro.commit(CommitParams {
            message: "nope".into(),
            ..CommitParams::default()
        })
        .await,
        ro.create_branch(CreateBranchParams {
            name: "nope".into(),
            ..CreateBranchParams::default()
        })
        .await,
    ] {
        assert!(result.unwrap_err().contains("--allow-writes"));
    }
    assert_eq!(git(d, &["rev-parse", "HEAD"]), head);
    assert!(git(d, &["status", "--porcelain"]).contains("?? b.txt"));
    assert!(!git(d, &["branch"]).contains("nope"));

    // With the flag they work.
    let rw = tools(&[d], true).await;
    assert_eq!(
        GitServer::new(tools(&[d], true).await).tool_names().len(),
        10
    );
    rw.stage(paths()).await.unwrap();
    assert!(git(d, &["status", "--porcelain"]).contains("A  b.txt"));
    rw.unstage(paths()).await.unwrap();
    rw.stage(paths()).await.unwrap();
    let out = rw
        .commit(CommitParams {
            message: "add b\n\nfrom mcp".into(),
            ..CommitParams::default()
        })
        .await
        .unwrap();
    assert!(out.starts_with("committed "), "{out}");
    assert_eq!(
        git(d, &["log", "-1", "--format=%s|%b"]).trim(),
        "add b|from mcp"
    );
    rw.create_branch(CreateBranchParams {
        name: "feature/x".into(),
        ..CreateBranchParams::default()
    })
    .await
    .unwrap();
    assert!(git(d, &["branch"]).contains("feature/x"));
    let bad = rw
        .create_branch(CreateBranchParams {
            name: "bad..name".into(),
            ..CreateBranchParams::default()
        })
        .await;
    assert!(bad.is_err());
    let empty = rw
        .commit(CommitParams {
            message: "   ".into(),
            ..CommitParams::default()
        })
        .await;
    assert!(empty.is_err());
}

#[tokio::test]
async fn large_output_is_truncated() {
    let dir = fixture();
    let d = dir.path();
    let big: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("a.txt"), big).unwrap();
    let t = tools(&[d], false).await;
    let diff = t.diff(DiffParams::default()).await.unwrap();
    assert!(diff.len() < mcp::tools::MAX_OUTPUT + 200, "{}", diff.len());
    assert!(diff.contains("[truncated:"));
}

/// Through the real MCP handler over an in-memory pipe: write tools are neither listed nor
/// callable without the flag.
#[tokio::test]
async fn handler_hides_write_tools() {
    use rmcp::ServiceExt as _;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let dir = fixture();
    let server = GitServer::new(tools(&[dir.path()], false).await);
    let (client, server_io) = tokio::io::duplex(1 << 16);
    let running = tokio::spawn(async move {
        let service = server.serve(server_io).await.expect("serves");
        _ = service.waiting().await;
    });
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"commit","arguments":{"message":"x"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    ];
    for request in requests {
        write.write_all(request.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
    }
    let mut replies = Vec::new();
    while replies.len() < 4 {
        let line = lines.next_line().await.unwrap().expect("a reply");
        replies.push(line);
    }
    let find = |id: &str| {
        replies
            .iter()
            .find(|r| r.contains(&format!("\"id\":{id},")))
            .cloned()
            .unwrap_or_default()
    };
    let list = find("2");
    assert!(list.contains(r#""name":"status""#), "{list}");
    for write in mcp::tools::WRITE_TOOLS {
        assert!(!list.contains(&format!(r#""name":"{write}""#)), "{list}");
    }
    assert!(find("3").contains("tool not found"), "{}", find("3"));
    assert!(find("4").contains("branch: main"), "{}", find("4"));
    drop(write);
    running.abort();
}
