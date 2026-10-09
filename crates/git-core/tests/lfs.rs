//! End-to-end with the real git-lfs (skipped when it is not installed): detection, attributes,
//! and the pointer change a worktree diff of an LFS file shows.

use std::path::Path;

use bstr::BString;
use git_core::lfs::{LfsAttrs, pointer_change};
use git_core::{CliRepo, DiffTarget, GitCommand, GitRepo, GitRunner};

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

fn lfs_installed() -> bool {
    let dir = tempfile::tempdir().unwrap();
    block(runner().run(GitCommand::read(dir.path()).args(["lfs", "version"]))).is_ok()
}

#[test]
fn lfs_files_diff_as_pointers() {
    if !lfs_installed() {
        eprintln!("git-lfs not installed: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["lfs", "install", "--local"]);
    std::fs::write(
        root.join(".gitattributes"),
        "*.uasset filter=lfs diff=lfs merge=lfs -text lockable\n",
    )
    .unwrap();
    std::fs::write(root.join("Hero.uasset"), vec![7u8; 3000]).unwrap();
    std::fs::write(root.join("notes.txt"), "hi\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "assets"]);

    let repo = block(CliRepo::open(runner(), root)).unwrap();
    let status = block(repo.lfs_status()).unwrap();
    assert!(status.used && status.lockable && !status.missing());
    assert!(status.version.unwrap().starts_with("git-lfs/"));

    let attrs =
        block(repo.lfs_attrs(&[BString::from("Hero.uasset"), BString::from("notes.txt")])).unwrap();
    assert_eq!(
        attrs.get(&BString::from("Hero.uasset")),
        Some(&LfsAttrs {
            lfs: true,
            lockable: true
        })
    );
    assert!(!attrs.contains_key(&BString::from("notes.txt")));

    // Lockable files are checked out read-only; make it writable like `git lfs lock` would.
    let hero = root.join("Hero.uasset");
    let mut perms = std::fs::metadata(&hero).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&hero, perms).unwrap();
    std::fs::write(&hero, vec![9u8; 5000]).unwrap();
    let diffs = block(repo.diff(DiffTarget::Unstaged { paths: vec![] })).unwrap();
    let change = pointer_change(&diffs[0]).expect("an LFS pointer diff");
    assert_eq!(change.old.unwrap().size, 3000);
    assert_eq!(change.new.unwrap().size, 5000);
}
