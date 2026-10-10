//! A pull request's Changes on real repositories: the head (or merge preview) is fetched from a
//! local bare "host" into `refs/gitdragon/pr/<n>/...`, touching no branch, and then diffed
//! from the merge base; the fetched refs are reused while the head is unchanged, left out of
//! the graph and pruned when old.

use std::path::Path;
use std::time::Duration;

use git_core::parse::diff::FileChange;
use git_core::pr_ops::{PrFetch, pr_base_ref, pr_head_ref};
use git_core::{CliRepo, GitCommand, GitRepo, GitRunner, LogQuery};

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

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = GitCommand::write(dir);
    for c in CONFIG {
        cmd = cmd.args(["-c", c]);
    }
    for (key, value) in env {
        cmd = cmd.env(*key, *value);
    }
    let out = block(runner().run(cmd.args(args))).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8(out).unwrap().trim().to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

struct Host {
    _dir: tempfile::TempDir,
    clone: CliRepo,
    clone_path: std::path::PathBuf,
    /// Head of pull request 7 (`refs/pull/7/head`), committed in 2001.
    head: String,
    /// Where it forked from main.
    fork_point: String,
    /// `refs/pull/8/merge`: main merged with the head.
    merge: String,
    main_at_merge: String,
}

/// A bare "host" with `main`, `refs/pull/7/head` and the merge preview `refs/pull/8/merge`
/// (main moved on after the fork), and a clone of it with remote `origin`.
fn host() -> Host {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let bare = dir.path().join("host.git");
    let clone_path = dir.path().join("clone");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    let lines: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    write(&work, "f.txt", &lines);
    write(&work, "gone.txt", "bye\n");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    let fork_point = git(&work, &["rev-parse", "HEAD"]);

    git(&work, &["switch", "-q", "-c", "feature"]);
    write(&work, "f.txt", &lines.replace("l5\n", "five\n"));
    write(&work, "new.txt", "a\nb\n");
    git(&work, &["rm", "-q", "gone.txt"]);
    git(&work, &["add", "."]);
    let old = [
        ("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z"),
        ("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z"),
    ];
    git_env(&work, &["commit", "-q", "-m", "feature"], &old);
    let head = git(&work, &["rev-parse", "HEAD"]);

    git(&work, &["switch", "-q", "main"]);
    write(&work, "main.txt", "main moved\n");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "main moves on"]);
    let main_at_merge = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["switch", "-q", "-c", "preview"]);
    git(&work, &["merge", "-q", "--no-edit", "--no-ff", "feature"]);
    let merge = git(&work, &["rev-parse", "HEAD"]);

    git(dir.path(), &["init", "-q", "--bare", "host.git"]);
    let bare_url = bare.to_string_lossy().replace('\\', "/");
    git(
        &work,
        &[
            "push",
            "-q",
            &bare_url,
            "main:refs/heads/main",
            "feature:refs/pull/7/head",
            "preview:refs/pull/8/merge",
        ],
    );
    git(dir.path(), &["clone", "-q", &bare_url, "clone"]);
    let clone = block(CliRepo::open(runner(), &clone_path)).unwrap();
    Host {
        _dir: dir,
        clone,
        clone_path,
        head,
        fork_point,
        merge,
        main_at_merge,
    }
}

fn fetch(number: u64, source_ref: &str, merge_preview: bool) -> PrFetch<'_> {
    PrFetch {
        remote: "origin",
        source_ref,
        target_branch: "main",
        number,
        merge_preview,
        base_sha: None,
    }
}

#[test]
fn fetches_the_head_and_diffs_from_the_merge_base() {
    let h = host();
    let repo = &h.clone;
    let refs_before = git(
        &h.clone_path,
        &["for-each-ref", "refs/heads", "refs/remotes"],
    );

    assert_eq!(
        block(repo.cached_pr_range(7, &h.head, false, None)).unwrap(),
        None
    );
    let range = block(repo.fetch_pr(&fetch(7, "pull/7/head", false))).unwrap();
    assert_eq!(range.head, h.head);
    // Not main's tip: the commits main gained since the fork are no part of the PR.
    assert_eq!(range.base, h.fork_point);
    assert!(!range.merge_preview);

    // No branch, remote-tracking ref or FETCH_HEAD changed.
    let refs_after = git(
        &h.clone_path,
        &["for-each-ref", "refs/heads", "refs/remotes"],
    );
    assert_eq!(refs_before, refs_after);
    assert!(!h.clone_path.join(".git").join("FETCH_HEAD").exists());
    assert_eq!(git(&h.clone_path, &["rev-parse", &pr_head_ref(7)]), h.head);
    assert_eq!(
        git(&h.clone_path, &["rev-parse", &pr_base_ref(7)]),
        h.main_at_merge
    );

    let files = block(repo.range_files(&range.base, &range.head, true)).unwrap();
    let summary: Vec<(String, FileChange, Option<u32>, Option<u32>)> = files
        .iter()
        .map(|f| {
            (
                String::from_utf8_lossy(f.path()).into_owned(),
                f.change,
                f.added,
                f.removed,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("f.txt".into(), FileChange::Modified, Some(1), Some(1)),
            ("gone.txt".into(), FileChange::Deleted, Some(0), Some(1)),
            ("new.txt".into(), FileChange::Added, Some(2), Some(0)),
        ]
    );
    let diff = block(repo.range_file_diff(&range.base, &range.head, &files[0]))
        .unwrap()
        .expect("a diff");
    assert_eq!(diff.hunks.len(), 1);

    // Same head: reused without a fetch.
    let cached = block(repo.cached_pr_range(7, &h.head, false, None)).unwrap();
    assert_eq!(cached, Some(range.clone()));
    // A new head (force-push): fetch again.
    let other = "0".repeat(40);
    assert_eq!(
        block(repo.cached_pr_range(7, &other, false, None)).unwrap(),
        None
    );
    assert_eq!(
        block(repo.cached_pr_range(7, "short", false, None)).unwrap(),
        None
    );

    // The graph leaves the fetched pull request out.
    let log = block(repo.log(LogQuery::graph())).unwrap();
    assert!(log.iter().all(|c| c.id != h.head));
    assert!(
        log.iter()
            .any(|c| c.id == h.main_at_merge || c.id == h.fork_point)
    );
}

#[test]
fn merge_preview_diffs_against_its_first_parent() {
    let h = host();
    let repo = &h.clone;
    let range = block(repo.fetch_pr(&fetch(8, "pull/8/merge", true))).unwrap();
    assert_eq!(range.head, h.merge);
    assert_eq!(range.base, h.main_at_merge);
    assert!(range.merge_preview);
    // The target branch is not fetched for a preview.
    assert!(
        block(repo.cached_pr_range(8, &h.head, true, None))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        block(repo.cached_pr_range(8, &h.merge, true, None)).unwrap(),
        None,
        "keyed by the head (the preview's second parent), not the preview"
    );
    let files = block(repo.range_files(&range.base, &range.head, false)).unwrap();
    assert_eq!(files.len(), 3);
}

#[test]
fn bad_input_and_missing_refs_fail() {
    let h = host();
    let repo = &h.clone;
    for (remote, source, target) in [
        ("origin", "-x", "main"),
        ("origin", "pull/../head", "main"),
        ("origin", "pull/7/head", "a..b"),
        ("origin", "pull/7/head", "-main"),
        ("a/b", "pull/7/head", "main"),
        ("--upload-pack=x", "pull/7/head", "main"),
    ] {
        let pr = PrFetch {
            remote,
            source_ref: source,
            target_branch: target,
            number: 7,
            merge_preview: false,
            base_sha: None,
        };
        assert!(
            block(repo.fetch_pr(&pr)).is_err(),
            "{remote} {source} {target}"
        );
    }
    assert!(block(repo.fetch_pr(&fetch(99, "pull/99/head", false))).is_err());
    assert_eq!(
        git(&h.clone_path, &["for-each-ref", "refs/gitdragon"]),
        "",
        "nothing fetched"
    );
}

#[test]
fn prunes_old_pull_requests_only() {
    let h = host();
    let repo = &h.clone;
    block(repo.fetch_pr(&fetch(7, "pull/7/head", false))).unwrap();
    block(repo.fetch_pr(&fetch(8, "pull/8/merge", true))).unwrap();
    // Nothing is a century old.
    let century = Duration::from_secs(100 * 365 * 86_400);
    assert_eq!(block(repo.prune_pr_refs(century)).unwrap(), 0);
    // PR 7's head dates from 2001; PR 8's preview is new.
    let month = Duration::from_secs(30 * 86_400);
    assert_eq!(block(repo.prune_pr_refs(month)).unwrap(), 1);
    let left = git(
        &h.clone_path,
        &["for-each-ref", "--format=%(refname)", "refs/gitdragon"],
    );
    assert_eq!(left, "refs/gitdragon/pr/8/head");
}

/// How the pull request ended up in main.
#[derive(Clone, Copy)]
enum Merged {
    Commit,
    FastForward,
    Squash,
}

/// A host whose PR 7 (`f.txt` changed, `new.txt` added) is already merged into main, main having
/// moved on afterwards. Answers the clone, the PR head and the host's base sha (main's tip
/// before the merge).
fn merged(how: Merged) -> (Host, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let bare = dir.path().join("host.git");
    let clone_path = dir.path().join("clone");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    write(
        &work, "f.txt", "one
two
",
    );
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    let fork_point = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["switch", "-q", "-c", "feature"]);
    write(
        &work, "f.txt", "one
TWO
",
    );
    write(
        &work, "new.txt", "a
",
    );
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "feature"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["switch", "-q", "main"]);
    if !matches!(how, Merged::FastForward) {
        // Main moved before the merge.
        write(
            &work,
            "other.txt",
            "x
",
        );
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "other"]);
    }
    let base = git(&work, &["rev-parse", "HEAD"]);
    match how {
        Merged::Commit => {
            git(&work, &["merge", "-q", "--no-edit", "--no-ff", "feature"]);
        }
        Merged::FastForward => {
            git(&work, &["merge", "-q", "--ff-only", "feature"]);
        }
        Merged::Squash => {
            git(&work, &["merge", "-q", "--squash", "feature"]);
            git(&work, &["commit", "-q", "-m", "squashed"]);
        }
    }
    // And on again afterwards.
    write(
        &work,
        "later.txt",
        "y
",
    );
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "later"]);

    git(dir.path(), &["init", "-q", "--bare", "host.git"]);
    let bare_url = bare.to_string_lossy().replace('\\', "/");
    git(
        &work,
        &[
            "push",
            "-q",
            &bare_url,
            "main:refs/heads/main",
            "feature:refs/pull/7/head",
        ],
    );
    git(dir.path(), &["clone", "-q", &bare_url, "clone"]);
    let clone = block(CliRepo::open(runner(), &clone_path)).unwrap();
    let host = Host {
        _dir: dir,
        clone,
        clone_path,
        head: head.clone(),
        fork_point,
        merge: String::new(),
        main_at_merge: base.clone(),
    };
    (host, head, base)
}

fn paths(h: &Host, base: &str, head: &str) -> Vec<String> {
    block(h.clone.range_files(base, head, false))
        .unwrap()
        .iter()
        .map(|f| String::from_utf8_lossy(f.path()).into_owned())
        .collect()
}

#[test]
fn merged_pull_requests_list_their_files_given_the_hosts_base() {
    for (how, name) in [
        (Merged::Commit, "merge commit"),
        (Merged::FastForward, "fast-forward"),
        (Merged::Squash, "squash"),
    ] {
        let (h, head, base_sha) = merged(how);
        let mut pr = fetch(7, "pull/7/head", false);
        pr.base_sha = Some(&base_sha);
        let range = block(h.clone.fetch_pr(&pr)).unwrap();
        assert_eq!(range.head, head, "{name}");
        assert_eq!(range.base, h.fork_point, "{name}");
        assert_eq!(
            paths(&h, &range.base, &range.head),
            ["f.txt", "new.txt"],
            "{name}"
        );
        // The cached range agrees.
        let cached = block(h.clone.cached_pr_range(7, &head, false, Some(&base_sha))).unwrap();
        assert_eq!(cached, Some(range), "{name}");
    }
}

#[test]
fn merged_pull_requests_without_a_base_are_empty_after_a_merge_commit() {
    // The old behavior, kept when the host gives no base sha: the head is an ancestor of the
    // target tip, so nothing is left to compare.
    let (h, head, _) = merged(Merged::Commit);
    let range = block(h.clone.fetch_pr(&fetch(7, "pull/7/head", false))).unwrap();
    assert_eq!(range.base, head);
    assert_eq!(range.base, range.head);
    // An unknown or malformed base falls back to the same.
    let missing = "1".repeat(40);
    for bad in [missing.as_str(), "not-a-sha", "--upload-pack=x"] {
        let mut pr = fetch(7, "pull/7/head", false);
        pr.base_sha = Some(bad);
        let range = block(h.clone.fetch_pr(&pr)).unwrap();
        assert_eq!(range.base, range.head, "{bad}");
    }
}
