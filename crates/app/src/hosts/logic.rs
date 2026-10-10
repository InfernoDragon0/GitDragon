//! The pure parts of the pull request / issue panels: list state, filters, state chips,
//! timestamps, the text a new pull request starts with and the checkout plan. No GPUI here,
//! so all of it is unit tested.

use ::hosts::{HostError, Issue, Page, PrState, PullRequest};
use git_core::parse::refs::{RefEntry, RefKind};

use crate::graph::format::relative_time;

// ---- list state -------------------------------------------------------------------------

/// What went wrong with the last load, in words the panel can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListError {
    pub message: String,
    /// Sign-in problem: Retry will not help, Settings > Accounts will.
    pub auth: bool,
}

impl ListError {
    pub fn of(err: &HostError) -> Self {
        Self {
            message: err.to_string(),
            auth: matches!(
                err,
                HostError::Unauthorized { .. }
                    | HostError::Forbidden { .. }
                    | HostError::MissingScope { .. }
            ),
        }
    }
}

/// One paged list as the panel shows it. A failed load keeps the items of the last good one
/// (stale data stays visible, with the error next to it).
#[derive(Debug)]
pub struct ListModel<T> {
    pub items: Vec<T>,
    /// The next page to ask for, when the host has one.
    pub next: Option<u32>,
    pub loading: bool,
    /// At least one load has succeeded since the last reset.
    pub loaded: bool,
    pub error: Option<ListError>,
    /// Unix seconds of the last successful load.
    pub updated: Option<i64>,
    req: u64,
}

impl<T> Default for ListModel<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next: None,
            loading: false,
            loaded: false,
            error: None,
            updated: None,
            req: 0,
        }
    }
}

impl<T> ListModel<T> {
    /// A load starts; the returned id must come back to [`Self::finish`].
    pub fn begin(&mut self) -> u64 {
        self.req += 1;
        self.loading = true;
        self.req
    }

    /// Forgets everything (another filter, repository or account); answers still on their
    /// way are ignored.
    pub fn reset(&mut self) {
        *self = Self {
            req: self.req + 1,
            ..Self::default()
        };
    }

    /// A load ended. `append` adds a further page to the items (skipping ones already
    /// there, pages shift while the list changes), otherwise the page replaces them. False
    /// when the answer is outdated and was ignored.
    pub fn finish(
        &mut self,
        req: u64,
        result: Result<Page<T>, HostError>,
        append: bool,
        key: impl Fn(&T) -> u64,
        now: i64,
    ) -> bool {
        if req != self.req {
            return false;
        }
        self.loading = false;
        match result {
            Ok(page) => {
                if append {
                    for item in page.items {
                        if !self.items.iter().any(|have| key(have) == key(&item)) {
                            self.items.push(item);
                        }
                    }
                } else {
                    self.items = page.items;
                }
                self.next = page.next;
                self.loaded = true;
                self.error = None;
                self.updated = Some(now);
            }
            Err(err) => self.error = Some(ListError::of(&err)),
        }
        true
    }
}

/// The row after (`down`) or before the one at `at` in a list of `len`, clamped at the ends;
/// the first row when none is selected; `None` for an empty list.
pub fn step(at: Option<usize>, len: usize, down: bool) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match at {
        Some(ix) if down => (ix + 1).min(last),
        Some(ix) => ix.saturating_sub(1).min(last),
        None => 0,
    })
}

// ---- filters and chips ------------------------------------------------------------------

/// The coloured state word on a pull request row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Open,
    Draft,
    Merged,
    Closed,
}

impl Chip {
    pub fn label(self) -> &'static str {
        match self {
            Chip::Open => "Open",
            Chip::Draft => "Draft",
            Chip::Merged => "Merged",
            Chip::Closed => "Closed",
        }
    }
}

/// A draft is only a draft while it is open.
pub fn chip(pr: &PullRequest) -> Chip {
    match pr.state {
        PrState::Open if pr.draft => Chip::Draft,
        PrState::Open => Chip::Open,
        PrState::Merged => Chip::Merged,
        PrState::Closed => Chip::Closed,
    }
}

/// The pull requests to show: all of them, or with `mine` only those `login` wrote.
pub fn visible_prs<'a>(items: &'a [PullRequest], mine: Option<&str>) -> Vec<&'a PullRequest> {
    items
        .iter()
        .filter(|pr| {
            mine.is_none_or(|me| {
                pr.author
                    .as_ref()
                    .is_some_and(|a| a.login.eq_ignore_ascii_case(me))
            })
        })
        .collect()
}

/// Issues matching every word of `text` in the title, number, labels or author (case
/// insensitive); everything when `text` is blank.
pub fn filter_issues<'a>(items: &'a [Issue], text: &str) -> Vec<&'a Issue> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    items
        .iter()
        .filter(|issue| {
            let labels = issue
                .labels
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let author = issue.author.as_ref().map_or("", |a| a.login.as_str());
            let hay = format!("#{} {} {labels} {author}", issue.number, issue.title).to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

// ---- time -------------------------------------------------------------------------------

/// Seconds since the epoch of an ISO 8601 / RFC 3339 timestamp (`2026-10-09T10:05:00Z`,
/// fractions and `+02:00` offsets accepted).
pub fn parse_iso(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', ' '])?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (time, zone) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut clock = time.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let second: i64 = match clock.next() {
        Some(s) => s.split('.').next()?.parse().ok()?,
        None => 0,
    };
    let offset = match zone.as_bytes().first() {
        Some(b'+' | b'-') => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let (h, m) = match digits.len() {
                2 => (digits.parse::<i64>().ok()?, 0),
                4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
                _ => return None,
            };
            sign * (h * 3600 + m * 60)
        }
        _ => 0,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `5 min ago` for a time in unix seconds.
pub fn relative(time: i64, now: i64) -> String {
    relative_time(time, 0, now)
}

/// `5 min ago`, `yesterday`, `2026-03-14`; the date part of `iso` when it cannot be read.
pub fn ago(iso: &str, now: i64) -> String {
    match parse_iso(iso) {
        Some(time) => relative_time(time, 0, now),
        None => iso.split(['T', ' ']).next().unwrap_or_default().to_string(),
    }
}

// ---- new pull request -------------------------------------------------------------------

/// Title and description a new pull request starts with, from the commits `base..compare`,
/// oldest first as `(subject, body)`: one commit gives its own message; several give the
/// first subject as title and every subject as a bullet. The caller leaves merge commits out.
pub fn prefill(commits: &[(String, String)]) -> (String, String) {
    match commits {
        [] => (String::new(), String::new()),
        [(subject, body)] => (subject.trim().to_string(), body.trim().to_string()),
        [(first, _), ..] => {
            let bullets: Vec<String> = commits
                .iter()
                .map(|(subject, _)| format!("- {}", subject.trim()))
                .collect();
            (first.trim().to_string(), bullets.join("\n"))
        }
    }
}

/// The branch a pull request most likely targets: the one the current branch tracks (when
/// that is another branch), else the branch the remote's HEAD points at, else the first of
/// main / master / develop that the remote has.
pub fn default_base(refs: &[RefEntry], remote: &str, current: Option<&str>) -> Option<String> {
    let prefix = format!("refs/remotes/{remote}/");
    let remote_branch = |name: &str| {
        refs.iter()
            .any(|r| r.full_name == format!("{prefix}{name}"))
    };

    let tracked = current.and_then(|current| {
        let local = refs
            .iter()
            .find(|r| r.kind == RefKind::LocalBranch && r.short_name == current)?;
        let upstream = local.upstream.as_ref()?.name.strip_prefix(&prefix)?;
        (upstream != current && upstream != "HEAD").then(|| upstream.to_string())
    });
    if tracked.is_some() {
        return tracked;
    }
    let head = refs.iter().find(|r| r.full_name == format!("{prefix}HEAD"));
    if let Some(head) = head {
        let same: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::RemoteBranch && r.oid == head.oid)
            .filter_map(|r| r.full_name.strip_prefix(&prefix))
            .filter(|name| *name != "HEAD")
            .collect();
        if let Some(name) = ["main", "master", "develop"]
            .iter()
            .find(|name| same.contains(name))
            .or(same.first())
        {
            return Some((*name).to_string());
        }
    }
    ["main", "master", "develop"]
        .into_iter()
        .find(|name| remote_branch(name))
        .map(str::to_string)
}

/// Branch names on `remote` (without `HEAD`), sorted.
pub fn remote_branches(refs: &[RefEntry], remote: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{remote}/");
    let mut names: Vec<String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::RemoteBranch)
        .filter_map(|r| r.full_name.strip_prefix(&prefix))
        .filter(|name| *name != "HEAD")
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

pub fn local_branches(refs: &[RefEntry]) -> Vec<String> {
    let mut names: Vec<String> = refs
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.short_name.clone())
        .collect();
    names.sort();
    names
}

/// The revision to count commits from: `remote/base` when the remote has it (so a stale
/// local `main` does not add commits), else `base` itself.
pub fn base_rev(refs: &[RefEntry], remote: &str, base: &str) -> String {
    let remote_ref = format!("refs/remotes/{remote}/{base}");
    if refs.iter().any(|r| r.full_name == remote_ref) {
        format!("{remote}/{base}")
    } else {
        base.to_string()
    }
}

/// Does the branch need pushing before a pull request can be opened from it?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOffer {
    /// Everything is on the remote already (or the branch is unknown).
    No,
    /// No upstream: the remote has no such branch yet.
    NoUpstream,
    /// Commits the upstream lacks.
    Ahead(u32),
}

pub fn push_offer(refs: &[RefEntry], branch: &str) -> PushOffer {
    let Some(local) = refs
        .iter()
        .find(|r| r.kind == RefKind::LocalBranch && r.short_name == branch)
    else {
        return PushOffer::No;
    };
    match &local.upstream {
        None => PushOffer::NoUpstream,
        Some(up) if up.gone => PushOffer::NoUpstream,
        Some(up) if up.ahead > 0 => PushOffer::Ahead(up.ahead),
        Some(_) => PushOffer::No,
    }
}

// ---- checkout ---------------------------------------------------------------------------

/// How to get a pull request's branch into the working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutPlan {
    /// Source branch in this repository: fetch it and track it. `fallback` is the host's
    /// head ref, tried when that fails (the branch was deleted, or it is a fork the host
    /// does not report).
    Branch {
        branch: String,
        fallback: Option<String>,
    },
    /// A fork: fetch the host's head ref into the local branch `pr/<number>`.
    HeadRef {
        head_ref: String,
    },
    Unavailable(String),
}

pub fn checkout_plan(pr: &PullRequest, head_ref: Option<String>) -> CheckoutPlan {
    if pr.source_repo.is_none() {
        return CheckoutPlan::Branch {
            branch: pr.source_branch.clone(),
            fallback: head_ref,
        };
    }
    match head_ref {
        Some(head_ref) => CheckoutPlan::HeadRef { head_ref },
        None => CheckoutPlan::Unavailable(
            "This pull request comes from a fork and the host has no ref to fetch it by.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::hosts::{IssueState, Label, Person};
    use git_core::parse::refs::Upstream;

    fn person(login: &str) -> Person {
        Person {
            login: login.into(),
            name: None,
            avatar_url: None,
            web_url: None,
        }
    }

    fn pr(number: u64, state: PrState, draft: bool, author: &str) -> PullRequest {
        PullRequest {
            number,
            title: format!("PR {number}"),
            body: None,
            state,
            draft,
            author: Some(person(author)),
            source_branch: format!("feat/{number}"),
            source_repo: None,
            target_branch: "main".into(),
            head_sha: format!("sha{number}"),
            base_sha: None,
            labels: vec![],
            web_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn issue(number: u64, title: &str, labels: &[&str], author: &str) -> Issue {
        Issue {
            number,
            title: title.into(),
            body: None,
            state: IssueState::Open,
            author: Some(person(author)),
            assignees: vec![],
            labels: labels
                .iter()
                .map(|l| Label {
                    name: (*l).into(),
                    color: None,
                })
                .collect(),
            comments: 0,
            web_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn reference(full: &str, kind: RefKind, oid: &str, upstream: Option<Upstream>) -> RefEntry {
        RefEntry {
            full_name: full.into(),
            short_name: full
                .strip_prefix("refs/heads/")
                .or(full.strip_prefix("refs/remotes/"))
                .unwrap_or(full)
                .into(),
            kind,
            oid: oid.into(),
            peeled_oid: None,
            is_head: false,
            upstream,
            date: 0,
            subject: Default::default(),
        }
    }

    fn local(name: &str, upstream: Option<Upstream>) -> RefEntry {
        reference(
            &format!("refs/heads/{name}"),
            RefKind::LocalBranch,
            "1",
            upstream,
        )
    }

    fn remote(name: &str, oid: &str) -> RefEntry {
        reference(
            &format!("refs/remotes/origin/{name}"),
            RefKind::RemoteBranch,
            oid,
            None,
        )
    }

    fn up(name: &str, ahead: u32, gone: bool) -> Upstream {
        Upstream {
            name: format!("refs/remotes/origin/{name}"),
            ahead,
            behind: 0,
            gone,
        }
    }

    #[test]
    fn keyboard_steps() {
        assert_eq!(step(None, 0, true), None);
        assert_eq!(step(None, 3, false), Some(0));
        assert_eq!(step(Some(0), 3, true), Some(1));
        assert_eq!(step(Some(2), 3, true), Some(2));
        assert_eq!(step(Some(0), 3, false), Some(0));
        assert_eq!(step(Some(2), 3, false), Some(1));
    }

    #[test]
    fn list_model_pages_and_stale_errors() {
        let mut list = ListModel::<u64>::default();
        let page = |items: Vec<u64>, next| Page::new(items, next);
        let r = list.begin();
        assert!(list.loading);
        assert!(list.finish(r, Ok(page(vec![1, 2], Some(2))), false, |n| *n, 100));
        assert_eq!(list.items, [1, 2]);
        assert_eq!((list.next, list.updated), (Some(2), Some(100)));

        // The next page shifts: 2 shows up again and is not doubled.
        let r = list.begin();
        assert!(list.finish(r, Ok(page(vec![2, 3], None)), true, |n| *n, 110));
        assert_eq!(list.items, [1, 2, 3]);
        assert_eq!(list.next, None);

        // A failed refresh keeps the old items and says why.
        let r = list.begin();
        let err = HostError::Unauthorized {
            host: "github.com".into(),
        };
        assert!(list.finish(r, Err(err), false, |n| *n, 120));
        assert_eq!(list.items, [1, 2, 3]);
        assert!(list.error.as_ref().unwrap().auth);
        assert_eq!(list.updated, Some(110));
        // ... and the next success clears it.
        let r = list.begin();
        list.finish(r, Ok(page(vec![9], None)), false, |n| *n, 130);
        assert_eq!(list.items, [9]);
        assert_eq!(list.error, None);

        // Answers that arrive after a reset are dropped.
        let r = list.begin();
        list.reset();
        assert!(!list.finish(r, Ok(page(vec![5], None)), false, |n| *n, 140));
        assert!(list.items.is_empty() && !list.loaded && !list.loading);
    }

    #[test]
    fn state_chips() {
        assert_eq!(chip(&pr(1, PrState::Open, false, "a")), Chip::Open);
        assert_eq!(chip(&pr(1, PrState::Open, true, "a")), Chip::Draft);
        assert_eq!(chip(&pr(1, PrState::Merged, true, "a")), Chip::Merged);
        assert_eq!(chip(&pr(1, PrState::Closed, false, "a")), Chip::Closed);
        assert_eq!(Chip::Draft.label(), "Draft");
    }

    #[test]
    fn mine_filter_is_case_insensitive() {
        let items = [
            pr(1, PrState::Open, false, "Me"),
            pr(2, PrState::Open, false, "you"),
            PullRequest {
                author: None,
                ..pr(3, PrState::Open, false, "x")
            },
        ];
        assert_eq!(visible_prs(&items, None).len(), 3);
        let mine = visible_prs(&items, Some("me"));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].number, 1);
    }

    #[test]
    fn issue_text_filter() {
        let items = [
            issue(7, "Crash on open", &["bug", "ui"], "ann"),
            issue(8, "Add dark mode", &["enhancement"], "bob"),
        ];
        let numbers = |text: &str| -> Vec<u64> {
            filter_issues(&items, text)
                .iter()
                .map(|i| i.number)
                .collect()
        };
        assert_eq!(numbers(""), [7, 8]);
        assert_eq!(numbers("  "), [7, 8]);
        assert_eq!(numbers("CRASH"), [7]);
        assert_eq!(numbers("bug"), [7]);
        assert_eq!(numbers("enhance dark"), [8]);
        assert_eq!(numbers("#8"), [8]);
        assert_eq!(numbers("bob"), [8]);
        assert!(numbers("crash dark").is_empty());
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-09T10:05:00Z"), Some(1_791_540_300));
        assert_eq!(parse_iso("2026-10-09T10:05:00.123Z"), Some(1_791_540_300));
        assert_eq!(parse_iso("2026-10-09T12:05:00+02:00"), Some(1_791_540_300));
        assert_eq!(parse_iso("2026-10-09T06:05:00-0400"), Some(1_791_540_300));
        assert_eq!(parse_iso("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_iso("nonsense"), None);
        assert_eq!(parse_iso("2026-13-09T10:05:00Z"), None);
        let now = 1_791_540_300 + 5 * 60;
        assert_eq!(ago("2026-10-09T10:05:00Z", now), "5 min ago");
        assert_eq!(ago("2026-10-09 garbage", now), "2026-10-09");
    }

    #[test]
    fn prefill_from_commits() {
        let c = |s: &str, b: &str| (s.to_string(), b.to_string());
        assert_eq!(prefill(&[]), (String::new(), String::new()));
        assert_eq!(
            prefill(&[c("Fix crash", "Because of X.\n\nMore.\n")]),
            ("Fix crash".into(), "Because of X.\n\nMore.".into())
        );
        assert_eq!(
            prefill(&[
                c("Add parser", "body one"),
                c("Wire it up", ""),
                c("Docs", "x")
            ]),
            (
                "Add parser".into(),
                "- Add parser\n- Wire it up\n- Docs".into()
            )
        );
    }

    #[test]
    fn base_follows_upstream_then_remote_head() {
        let head = reference(
            "refs/remotes/origin/HEAD",
            RefKind::RemoteBranch,
            "m1",
            None,
        );
        let refs = vec![
            local("feat", Some(up("feat", 0, false))),
            local("topic", Some(up("develop", 0, false))),
            head.clone(),
            remote("develop", "d1"),
            remote("main", "m1"),
            remote("feat", "f1"),
        ];
        // Tracks another branch: that is the base.
        assert_eq!(
            default_base(&refs, "origin", Some("topic")).as_deref(),
            Some("develop")
        );
        // Tracks itself: the remote's HEAD branch.
        assert_eq!(
            default_base(&refs, "origin", Some("feat")).as_deref(),
            Some("main")
        );
        // No HEAD ref: the conventional names.
        let refs = vec![
            local("feat", None),
            remote("develop", "d1"),
            remote("master", "m1"),
        ];
        assert_eq!(
            default_base(&refs, "origin", Some("feat")).as_deref(),
            Some("master")
        );
        assert_eq!(default_base(&[], "origin", None), None);
        assert_eq!(
            remote_branches(&[head, remote("b", "1"), remote("a", "1")], "origin"),
            ["a", "b"]
        );
        assert_eq!(
            local_branches(&[local("z", None), local("a", None)]),
            ["a", "z"]
        );
    }

    #[test]
    fn base_revision_prefers_the_remote_branch() {
        let refs = vec![local("main", None), remote("main", "1")];
        assert_eq!(base_rev(&refs, "origin", "main"), "origin/main");
        assert_eq!(base_rev(&refs, "origin", "dev"), "dev");
    }

    #[test]
    fn push_is_offered_for_unpushed_branches() {
        let refs = vec![
            local("new", None),
            local("gone", Some(up("gone", 0, true))),
            local("ahead", Some(up("ahead", 2, false))),
            local("synced", Some(up("synced", 0, false))),
        ];
        assert_eq!(push_offer(&refs, "new"), PushOffer::NoUpstream);
        assert_eq!(push_offer(&refs, "gone"), PushOffer::NoUpstream);
        assert_eq!(push_offer(&refs, "ahead"), PushOffer::Ahead(2));
        assert_eq!(push_offer(&refs, "synced"), PushOffer::No);
        assert_eq!(push_offer(&refs, "missing"), PushOffer::No);
    }

    #[test]
    fn checkout_plans() {
        let same = pr(5, PrState::Open, false, "a");
        let head = Some("pull/5/head".to_string());
        assert_eq!(
            checkout_plan(&same, head.clone()),
            CheckoutPlan::Branch {
                branch: "feat/5".into(),
                fallback: head.clone()
            }
        );
        assert_eq!(
            checkout_plan(&same, None),
            CheckoutPlan::Branch {
                branch: "feat/5".into(),
                fallback: None
            }
        );
        let fork = PullRequest {
            source_repo: Some("them/repo".into()),
            ..same
        };
        assert_eq!(
            checkout_plan(&fork, head),
            CheckoutPlan::HeadRef {
                head_ref: "pull/5/head".into()
            }
        );
        assert!(matches!(
            checkout_plan(&fork, None),
            CheckoutPlan::Unavailable(_)
        ));
    }
}
