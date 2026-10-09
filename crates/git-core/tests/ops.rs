//! End-to-end: stage / unstage / discard / partial patches / commit / stash on real repos.

use std::path::Path;

use bstr::BString;
use git_core::ops::{ApplyTarget, CommitRequest, StashAction};
use git_core::parse::status::{EntryKind, FileState};
use git_core::patch::{self, Direction, Pick};
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

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// One commit with `f.txt` (10 lines); identity set in the repo config so `commit` works.
fn fixture() -> (tempfile::TempDir, CliRepo) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    for c in CONFIG {
        let (key, value) = c.split_once('=').unwrap();
        git(d, &["config", key, value]);
    }
    let lines: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    write(d, "f.txt", &lines);
    git(d, &["add", "f.txt"]);
    git(d, &["commit", "-q", "-m", "first"]);
    let repo = block(CliRepo::open(runner(), d)).unwrap();
    (dir, repo)
}

fn paths(p: &[&str]) -> Vec<BString> {
    p.iter().map(|s| BString::from(*s)).collect()
}

/// Two separate hunks: line 1 and line 10 changed.
fn two_edits(d: &Path) {
    let text: String = (1..=10)
        .map(|i| match i {
            1 => "ONE\n".to_string(),
            10 => "TEN\n".to_string(),
            _ => format!("l{i}\n"),
        })
        .collect();
    write(d, "f.txt", &text);
}

#[test]
fn stage_unstage_discard_files() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "f.txt", "changed\n");
    write(d, "new.txt", "n\n");
    block(repo.stage(&paths(&["f.txt", "new.txt"]))).unwrap();
    let st = block(repo.status()).unwrap();
    assert!(st.entries.iter().all(|e| e.index != FileState::Unmodified));

    block(repo.unstage(&paths(&["f.txt", "new.txt"]))).unwrap();
    let st = block(repo.status()).unwrap();
    assert!(st.entries.iter().all(|e| e.index == FileState::Unmodified));
    assert!(st.entries.iter().any(|e| e.kind == EntryKind::Untracked));

    block(repo.discard(&paths(&["f.txt"]))).unwrap();
    assert!(read(d, "f.txt").starts_with("l1\n"));
    block(repo.delete_untracked(&paths(&["new.txt"]))).unwrap();
    assert!(!d.join("new.txt").exists());
    assert!(block(repo.status()).unwrap().entries.is_empty());
}

#[test]
fn untracked_file_as_all_added() {
    let (dir, repo) = fixture();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    write(dir.path(), "sub/new file.txt", "a\nb\n");
    let files = block(repo.untracked_diff(&BString::from("sub/new file.txt"))).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].new_path, Some(BString::from("sub/new file.txt")));
    assert_eq!(files[0].hunks[0].lines.len(), 2);
    assert!(block(repo.untracked_diff(&BString::from("../x"))).is_err());
}

#[test]
fn rejects_paths_outside_the_worktree() {
    let (_dir, repo) = fixture();
    for bad in ["../x", "/tmp/x", ""] {
        assert!(block(repo.discard(&paths(&[bad]))).is_err(), "{bad}");
    }
    assert!(block(repo.stage(&[])).is_err());
}

#[test]
fn stage_and_unstage_one_hunk() {
    let (dir, repo) = fixture();
    two_edits(dir.path());
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    assert_eq!(file.hunks.len(), 2);
    let p = patch::build(&file, &[(1, Pick::All)], Direction::Forward).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Forward)).unwrap();

    let staged = block(repo.diff(DiffTarget::Staged { paths: vec![] })).unwrap();
    assert_eq!(staged[0].hunks.len(), 1);
    assert!(staged[0].hunks[0].lines.iter().any(|l| l.content == "TEN"));
    let unstaged = block(repo.diff(DiffTarget::Unstaged { paths: vec![] })).unwrap();
    assert!(
        unstaged[0].hunks[0]
            .lines
            .iter()
            .any(|l| l.content == "ONE")
    );
    assert_eq!(unstaged[0].hunks.len(), 1);

    // And back out of the index.
    let p = patch::build(&staged[0], &[(0, Pick::All)], Direction::Reverse).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Reverse)).unwrap();
    assert!(
        block(repo.diff(DiffTarget::Staged { paths: vec![] }))
            .unwrap()
            .is_empty()
    );
    assert_eq!(read(dir.path(), "f.txt").lines().next(), Some("ONE"));
}

#[test]
fn stage_single_line_then_discard_the_rest() {
    let (dir, repo) = fixture();
    let d = dir.path();
    // l2 -> two lines: stage only the first added line.
    let text = read(d, "f.txt").replace("l2\n", "A\nB\n");
    write(d, "f.txt", &text);
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    let added = file.hunks[0]
        .lines
        .iter()
        .position(|l| l.content == "A")
        .unwrap();
    let p = patch::build(&file, &[(0, Pick::Lines(vec![added]))], Direction::Forward).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Index, Direction::Forward)).unwrap();
    let index = git(d, &["show", ":f.txt"]);
    assert!(index.starts_with("l1\nl2\nA\nl3\n"), "{index}");

    // Discard what is left unstaged: the worktree matches the index again.
    let file = block(repo.diff(DiffTarget::Unstaged { paths: vec![] }))
        .unwrap()
        .remove(0);
    let p = patch::build(&file, &[(0, Pick::All)], Direction::Reverse).unwrap();
    block(repo.apply_patch(p, ApplyTarget::Worktree, Direction::Reverse)).unwrap();
    assert_eq!(read(d, "f.txt"), index);
}

#[test]
fn commit_amend_and_trailers() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "g.txt", "g\n");
    block(repo.stage(&paths(&["g.txt"]))).unwrap();
    let req = CommitRequest {
        summary: "Add g".into(),
        body: "# not a comment".into(),
        signoff: true,
        author: Some("Ann <ann@x.io>".into()),
        co_authors: vec!["Bo <bo@x.io>".into()],
        ..Default::default()
    };
    let id = block(repo.commit(&req)).unwrap();
    assert_eq!(id.len(), 40);
    let msg = block(repo.head_message()).unwrap().unwrap();
    assert!(msg.starts_with("Add g\n\n# not a comment"), "{msg}");
    assert!(msg.contains("Co-authored-by: Bo <bo@x.io>"), "{msg}");
    assert!(
        msg.contains("Signed-off-by: Test <test@example.com>"),
        "{msg}"
    );
    assert_eq!(git(d, &["log", "-1", "--format=%an"]).trim(), "Ann");

    let amend = CommitRequest {
        summary: "Add g (amended)".into(),
        amend: true,
        ..Default::default()
    };
    block(repo.commit(&amend)).unwrap();
    assert_eq!(git(d, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert!(
        block(repo.commit(&CommitRequest::default())).is_err(),
        "empty summary"
    );
}

#[test]
fn stash_push_pop_drop() {
    let (dir, repo) = fixture();
    let d = dir.path();
    write(d, "f.txt", "stashed\n");
    write(d, "u.txt", "u\n");
    block(repo.stash_push("wip", true, false, &[])).unwrap();
    assert!(block(repo.status()).unwrap().entries.is_empty());
    assert_eq!(block(repo.stashes()).unwrap().len(), 1);

    block(repo.stash_action(StashAction::Apply, 0)).unwrap();
    assert_eq!(read(d, "f.txt"), "stashed\n");
    assert_eq!(block(repo.stashes()).unwrap().len(), 1);
    block(repo.stash_action(StashAction::Drop, 0)).unwrap();
    assert!(block(repo.stashes()).unwrap().is_empty());

    block(repo.stash_push("", false, false, &paths(&["f.txt"]))).unwrap();
    assert!(d.join("u.txt").exists());
    block(repo.stash_action(StashAction::Pop, 0)).unwrap();
    assert_eq!(read(d, "f.txt"), "stashed\n");
}
