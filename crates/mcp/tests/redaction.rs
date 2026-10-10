//! Redaction canary for tool output. Its own test binary: the redaction registry is process-wide.

use std::path::Path;
use std::process::Command;

use mcp::tools::{BlameParams, DiffKind, DiffParams, LogParams, RepoParams, ShowParams};
use mcp::{Config, GitTools};

const CANARY: &str = "CANARY-mcp-redaction-0123456789abcdef";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[tokio::test]
async fn canary_never_leaves_a_tool() {
    git_core::redact::register(CANARY);
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    let secret_line = format!("token = {CANARY}\nurl = https://me:pw@example.com/r.git\n");
    std::fs::write(d.join("conf.txt"), &secret_line).unwrap();
    git(d, &["add", "."]);
    git(
        d,
        &[
            "commit",
            "-q",
            "-m",
            &format!("add {CANARY}\n\nbody {CANARY}"),
        ],
    );
    std::fs::write(d.join("conf.txt"), format!("{secret_line}again {CANARY}\n")).unwrap();
    std::fs::write(d.join(format!("{CANARY}.txt")), "x").unwrap();
    git(d, &["branch", &format!("b-{CANARY}")]);

    let config = Config {
        repos: vec![d.to_path_buf()],
        allow_writes: false,
    };
    let t = GitTools::open(&config, git_core::GitRunner::detect().unwrap())
        .await
        .unwrap();
    let outputs = [
        t.status(RepoParams::default()).await,
        t.log(LogParams::default()).await,
        t.show(ShowParams::default()).await,
        t.diff(DiffParams::default()).await,
        t.diff(DiffParams {
            target: DiffKind::Commit,
            rev: Some("HEAD".into()),
            ..DiffParams::default()
        })
        .await,
        t.blame(BlameParams {
            path: "conf.txt".into(),
            ..BlameParams::default()
        })
        .await,
        t.branches(Default::default()).await,
        // Errors echo their input: still masked.
        t.log(LogParams {
            rev: Some(format!("{CANARY}-missing")),
            ..LogParams::default()
        })
        .await,
        t.blame(BlameParams {
            path: format!("../{CANARY}"),
            ..BlameParams::default()
        })
        .await,
    ];
    let mut saw_mask = 0;
    for out in outputs {
        let text = match out {
            Ok(t) | Err(t) => t,
        };
        assert!(!text.contains(CANARY), "{text}");
        assert!(!text.contains(":pw@"), "{text}");
        saw_mask += usize::from(text.contains(git_core::redact::MASK));
    }
    assert!(saw_mask >= 7, "the canary was in the data: {saw_mask}");
}
