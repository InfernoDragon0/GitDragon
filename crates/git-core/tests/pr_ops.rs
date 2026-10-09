//! End-to-end: checking out pull request branches against a bare remote in a temp dir.

use std::path::Path;

use git_core::{CliRepo, GitCommand, GitRunner};

fn block<F: std::future::Future>(f: F) -> F::Output {
    async_io::block_on(f)
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

const CONFIG: &[&str] = &[
    "user.name=Test",
    "user.email=test@example.com",
    "init.defaultBranch=main",
    "core.autocrlf=false",
    "commit.gpgsign=false",
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = GitCommand::write(dir);
    for c in CONFIG {
        cmd = cmd.args(["-c", c]);
    }
    let out = block(runner().run(cmd.args(args))).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap()
}

fn commit(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", file]);
}

fn rev(dir: &Path, r: &str) -> String {
    git(dir, &["rev-parse", r]).trim().to_string()
}

fn url(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A bare remote with `main`, and a clone (`work`) of it with identity configured.
fn setup() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    CliRepo,
) {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("remote.git");
    let seed = root.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(root.path(), &["init", "-q", "--bare", "remote.git"]);
    git(&seed, &["init", "-q"]);
    commit(&seed, "a.txt", "a\n");
    git(&seed, &["remote", "add", "origin", &url(&bare)]);
    git(&seed, &["push", "-q", "origin", "main"]);
    let work = root.path().join("work");
    git(root.path(), &["clone", "-q", &url(&bare), "work"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(&work, &["config", key, value]);
    }
    let repo = block(CliRepo::open(runner(), &work)).unwrap();
    (root, bare, work, repo)
}

#[test]
fn same_repo_pull_request_branch() {
    let (root, bare, work, repo) = setup();
    // Someone pushes feat/x after we cloned.
    let other = root.path().join("other");
    git(root.path(), &["clone", "-q", &url(&bare), "other"]);
    git(&other, &["switch", "-q", "-c", "feat/x"]);
    commit(&other, "x.txt", "x\n");
    git(&other, &["push", "-q", "origin", "feat/x"]);

    let name = block(repo.checkout_remote_branch("origin", "feat/x")).unwrap();
    assert_eq!(name, "feat/x");
    assert_eq!(rev(&work, "HEAD"), rev(&other, "HEAD"));
    assert_eq!(git(&work, &["branch", "--show-current"]).trim(), "feat/x");
    assert_eq!(
        git(&work, &["rev-parse", "--abbrev-ref", "feat/x@{upstream}"]).trim(),
        "origin/feat/x"
    );

    // Existing local branch: just switched to.
    git(&work, &["switch", "-q", "main"]);
    block(repo.checkout_remote_branch("origin", "feat/x")).unwrap();
    assert_eq!(git(&work, &["branch", "--show-current"]).trim(), "feat/x");

    // Names git would refuse never reach git.
    assert!(block(repo.checkout_remote_branch("origin", "bad name")).is_err());
    assert!(block(repo.checkout_remote_branch("origin", "-x")).is_err());
    assert!(block(repo.checkout_remote_branch("bad remote", "main")).is_err());
    // A branch the remote lacks fails in fetch.
    assert!(block(repo.checkout_remote_branch("origin", "nope")).is_err());
}

#[test]
fn fork_pull_request_through_the_hosts_ref() {
    let (root, bare, work, repo) = setup();
    // The fork's branch is only reachable as refs/pull/5/head on the remote.
    let other = root.path().join("other");
    git(root.path(), &["clone", "-q", &url(&bare), "other"]);
    commit(&other, "f.txt", "f\n");
    let first = rev(&other, "HEAD");
    git(&other, &["push", "-q", "origin", "HEAD:refs/pull/5/head"]);

    let name = block(repo.checkout_pull_ref("origin", "pull/5/head", 5)).unwrap();
    assert_eq!(name, "pr/5");
    assert_eq!(rev(&work, "HEAD"), first);
    assert_eq!(git(&work, &["branch", "--show-current"]).trim(), "pr/5");

    // The pull request gets another commit: the local branch fast-forwards.
    commit(&other, "g.txt", "g\n");
    git(&other, &["push", "-q", "origin", "HEAD:refs/pull/5/head"]);
    block(repo.checkout_pull_ref("origin", "pull/5/head", 5)).unwrap();
    assert_eq!(rev(&work, "HEAD"), rev(&other, "HEAD"));

    // Bad refs and numbers without a ref fail without touching the checkout.
    assert!(block(repo.checkout_pull_ref("origin", "../x", 5)).is_err());
    assert!(block(repo.checkout_pull_ref("origin", "-x", 5)).is_err());
    assert!(block(repo.checkout_pull_ref("origin", "pull/6/head", 6)).is_err());
    assert_eq!(git(&work, &["branch", "--show-current"]).trim(), "pr/5");
}
