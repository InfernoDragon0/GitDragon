//! The conversation of the open issue or pull request: its timeline (comments as Markdown,
//! one-line events), and for pull requests the reviews and the inline review comments grouped
//! by file and line. Embedded by the issue and pull request pages as a child view.
//!
//! Loading, so the page never waits on one slow call:
//! - The page header and description come from the list data already in memory; this view
//!   only adds the sections below them.
//! - Each section is fetched on its own task, all at once, and paints when its first page
//!   lands (a skeleton until then). An error stays inside its section.
//! - Paged sections paint page 1 first. When the host names the last page, the rest are asked
//!   for together; otherwise one after another (each answer says whether another follows).
//!   At most [`MAX_PAGES`] pages per round; "Show more" asks for the next round.
//! - Conversations stay in memory per thread (the last [`KEPT`]): re-opening one paints it at
//!   once and revalidates every loaded page in the background (an ETag 304 costs no quota).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;

use ::hosts::host::HostResult;
use ::hosts::{
    BodyFormat, DiffSide, EventKind, Feature, GitHost, HostError, Page, Person, RepoId, Review,
    ReviewComment, ReviewState, Thread, ThreadComment, TimelineEvent, TimelineItem, WebTarget,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ElementId, Global, Hsla, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use ui::Palette;

use super::error_banner;
use super::logic::{self, ListError};
use crate::hosted::Session;

/// Pages asked for per round (100 items each): a 1000-comment thread loads whole, anything
/// longer offers "Show more".
pub const MAX_PAGES: u32 = 10;
/// Conversations kept in memory.
const KEPT: usize = 24;
/// Longest comment body drawn; the rest is on the web.
const MAX_BODY: usize = 20_000;
/// Diff hunk lines shown above an inline comment (the end of the hunk is the commented line).
const HUNK_LINES: usize = 6;

// ---- one paged section ----------------------------------------------------------------------

/// One section's pages as they arrive. Pages are kept by number, so answers can land in any
/// order; a revalidation keeps showing the old pages until their new answers replace them.
#[derive(Debug)]
pub struct Section<T> {
    pages: BTreeMap<u32, Vec<T>>,
    pending: BTreeSet<u32>,
    /// The host named the last page (page 1's answer this round): the rest go out at once.
    last: Option<u32>,
    /// Pages up to this are fetched without asking.
    cap: u32,
    /// More pages exist beyond `cap`.
    pub more: bool,
    /// Page 1 arrived at least once.
    pub loaded: bool,
    pub error: Option<ListError>,
    /// The host has no such thing; the section is hidden.
    pub unsupported: bool,
    generation: u64,
}

impl<T> Default for Section<T> {
    fn default() -> Self {
        Self {
            pages: BTreeMap::new(),
            pending: BTreeSet::new(),
            last: None,
            cap: MAX_PAGES,
            more: false,
            loaded: false,
            error: None,
            unsupported: false,
            generation: 0,
        }
    }
}

impl<T> Section<T> {
    /// Every loaded item, page by page.
    pub fn items(&self) -> impl Iterator<Item = &T> {
        self.pages.values().flatten()
    }

    pub fn len(&self) -> usize {
        self.pages.values().map(Vec::len).sum()
    }

    /// Nothing to show yet: the skeleton.
    pub fn loading_first(&self) -> bool {
        !self.loaded && !self.pending.is_empty()
    }

    /// Pages not seen before are on their way.
    pub fn loading_more(&self) -> bool {
        self.loaded && self.pending.iter().any(|p| !self.pages.contains_key(p))
    }

    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A (re)load starts: page 1 is asked for again, the old pages stay until replaced.
    /// Returns the generation the answers must carry.
    pub fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.pending.clear();
        self.pending.insert(1);
        self.last = None;
        self.error = None;
        self.generation
    }

    /// Page `page` of round `generation` answered; returns the pages to ask for next.
    pub fn arrived(&mut self, generation: u64, page: u32, result: HostResult<Page<T>>) -> Vec<u32> {
        if generation != self.generation || !self.pending.remove(&page) {
            return Vec::new();
        }
        let answer = match result {
            Ok(answer) => answer,
            Err(HostError::Unsupported(_)) => {
                self.unsupported = true;
                self.pending.clear();
                return Vec::new();
            }
            Err(err) => {
                self.error = Some(ListError::of(&err));
                return Vec::new();
            }
        };
        self.pages.insert(page, answer.items);
        if page == 1 {
            self.loaded = true;
            if let Some(last) = answer.last.filter(|_| answer.next.is_some()) {
                // Parallel: everything up to the cap now.
                self.last = Some(last);
                self.pages.retain(|p, _| *p <= last);
                self.more = last > self.cap;
                return self.ask((2..=last.min(self.cap)).collect());
            }
        } else if self.last.is_some() {
            return Vec::new();
        }
        // One after another.
        match answer.next {
            Some(next) if next <= self.cap => self.ask(vec![next]),
            Some(_) => {
                self.more = true;
                Vec::new()
            }
            None => {
                self.more = false;
                self.pages.retain(|p, _| *p <= page);
                Vec::new()
            }
        }
    }

    /// "Show more": the next round of pages.
    pub fn show_more(&mut self) -> Vec<u32> {
        if !self.more {
            return Vec::new();
        }
        let from = self.cap + 1;
        self.cap += MAX_PAGES;
        match self.last {
            Some(last) => {
                self.more = last > self.cap;
                self.ask((from..=last.min(self.cap)).collect())
            }
            None => {
                self.more = false;
                self.ask(vec![from])
            }
        }
    }

    fn ask(&mut self, pages: Vec<u32>) -> Vec<u32> {
        let fresh: Vec<u32> = pages
            .into_iter()
            .filter(|p| self.pending.insert(*p))
            .collect();
        fresh
    }

    /// Adds `item` at the end (a comment just posted), before the reload brings it.
    pub fn push(&mut self, item: T) {
        let last = self.pages.keys().next_back().copied().unwrap_or(1);
        self.pages.entry(last).or_default().push(item);
    }
}

/// The three sections of one thread.
#[derive(Default)]
struct ThreadState {
    timeline: Section<TimelineItem>,
    reviews: Section<Review>,
    comments: Section<ReviewComment>,
}

impl ThreadState {
    /// Everyone seen so far, in order of appearance: comment authors and event actors, review
    /// authors, inline commenters.
    fn people(&self) -> Vec<Person> {
        let timeline = self.timeline.items().filter_map(|i| match i {
            TimelineItem::Comment(c) => c.author.as_ref(),
            TimelineItem::Event(e) => e.actor.as_ref(),
        });
        let reviews = self.reviews.items().filter_map(|r| r.author.as_ref());
        let inline = self.comments.items().filter_map(|c| c.author.as_ref());
        participants(timeline.chain(reviews).chain(inline))
    }
}

/// `people` without repeats: by login, ignoring case, first appearance first. A later copy
/// that knows more (avatar, profile link) fills the gaps of the first.
pub fn participants<'a>(people: impl IntoIterator<Item = &'a Person>) -> Vec<Person> {
    let mut out: Vec<Person> = Vec::new();
    for person in people {
        if person.login.trim().is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|p| p.login.eq_ignore_ascii_case(&person.login))
        {
            Some(known) => {
                if known.avatar_url.is_none() {
                    known.avatar_url = person.avatar_url.clone();
                }
                if known.web_url.is_none() {
                    known.web_url = person.web_url.clone();
                }
                if known.name.is_none() {
                    known.name = person.name.clone();
                }
            }
            None => out.push(person.clone()),
        }
    }
    out
}

/// The participants of the threads shown, for the page's side column (which cannot reach the
/// conversation view): written whenever a section arrives.
#[derive(Default)]
struct ThreadPeople {
    /// Given when the page opens (author, assignees): they come first.
    head: HashMap<Thread, Vec<Person>>,
    /// Seen in the loaded sections.
    seen: HashMap<Thread, Vec<Person>>,
}

impl Global for ThreadPeople {}

/// The participants known so far of `thread`.
pub fn thread_people(thread: Thread, cx: &App) -> Vec<Person> {
    let Some(g) = cx.try_global::<ThreadPeople>() else {
        return Vec::new();
    };
    participants(
        g.head
            .get(&thread)
            .into_iter()
            .chain(g.seen.get(&thread))
            .flatten(),
    )
}

/// Puts `people` (the author, then assignees, ...) in front of the participants of `thread`
/// and asks for their images. The pages call this on every render: it only acts on a change.
pub fn seed_head(thread: Thread, people: Vec<Person>, cx: &mut App) {
    if !cx.has_global::<ThreadPeople>() {
        cx.set_global(ThreadPeople::default());
    }
    let head = participants(people.iter());
    if cx.global::<ThreadPeople>().head.get(&thread) == Some(&head) {
        return;
    }
    request_faces(&head, cx);
    cx.global_mut::<ThreadPeople>().head.insert(thread, head);
}

/// Asks the avatar pipeline for the first [`FACES`] of `people`.
fn request_faces(people: &[Person], cx: &mut App) {
    let Some(avatars) = crate::avatars::handle(cx) else {
        return;
    };
    avatars.update(cx, |a, cx| {
        for person in people.iter().take(FACES) {
            match (avatar_key(person), person.avatar_url.as_deref()) {
                (Some(key), _) => a.request([key.as_str()], cx),
                (None, Some(url)) => _ = a.request_url(url, cx),
                (None, None) => {}
            }
        }
    });
}

/// How many faces get an image requested; the rest of a long list are only counted.
pub const FACES: usize = 16;

/// The pseudo noreply email under which [`crate::avatars`] knows a GitHub user, from the
/// host's own avatar URL (`.../u/<id>`) or, failing that, the login. Other hosts: `None`
/// (initials), since that pipeline only has free GitHub sources.
pub fn avatar_key(person: &Person) -> Option<String> {
    let github_id = person
        .avatar_url
        .as_deref()
        .and_then(|u| u.strip_prefix("https://avatars.githubusercontent.com/u/"))
        .map(|rest| rest.split(['?', '/']).next().unwrap_or_default())
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
    if let Some(id) = github_id {
        return Some(format!("{id}+{}@users.noreply.github.com", person.login));
    }
    let on_github = person
        .web_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://github.com/"));
    on_github.then(|| format!("{}@users.noreply.github.com", person.login))
}

/// A section's element type, how to fetch a page of it, and where it lives.
trait Part: Sized + 'static {
    const FEATURE: Feature;
    fn fetch(
        host: Arc<dyn GitHost>,
        repo: RepoId,
        thread: Thread,
        page: u32,
    ) -> impl Future<Output = HostResult<Page<Self>>> + 'static;
    fn section(state: &mut ThreadState) -> &mut Section<Self>;
}

impl Part for TimelineItem {
    const FEATURE: Feature = Feature::Timeline;
    async fn fetch(
        host: Arc<dyn GitHost>,
        repo: RepoId,
        thread: Thread,
        page: u32,
    ) -> HostResult<Page<Self>> {
        host.timeline(&repo, thread, page).await
    }
    fn section(state: &mut ThreadState) -> &mut Section<Self> {
        &mut state.timeline
    }
}

impl Part for Review {
    const FEATURE: Feature = Feature::Reviews;
    async fn fetch(
        host: Arc<dyn GitHost>,
        repo: RepoId,
        thread: Thread,
        page: u32,
    ) -> HostResult<Page<Self>> {
        host.pr_reviews(&repo, thread.number(), page).await
    }
    fn section(state: &mut ThreadState) -> &mut Section<Self> {
        &mut state.reviews
    }
}

impl Part for ReviewComment {
    const FEATURE: Feature = Feature::ReviewComments;
    async fn fetch(
        host: Arc<dyn GitHost>,
        repo: RepoId,
        thread: Thread,
        page: u32,
    ) -> HostResult<Page<Self>> {
        host.pr_review_comments(&repo, thread.number(), page).await
    }
    fn section(state: &mut ThreadState) -> &mut Section<Self> {
        &mut state.comments
    }
}

// ---- the view -----------------------------------------------------------------------------

pub struct ConversationView {
    host: Option<Arc<dyn GitHost>>,
    repo: Option<RepoId>,
    current: Option<Thread>,
    threads: HashMap<Thread, ThreadState>,
    /// Least recently shown first.
    order: VecDeque<Thread>,
    /// Re-render when avatar images arrive.
    watching_avatars: bool,
}

impl ConversationView {
    pub fn new() -> Self {
        Self {
            host: None,
            repo: None,
            current: None,
            threads: HashMap::new(),
            order: VecDeque::new(),
            watching_avatars: false,
        }
    }

    /// Shows `thread`: a known one paints from memory and revalidates, a new one loads. The
    /// same thread again is a no-op, so the pages can call this on every render.
    pub fn show(&mut self, session: &Session, thread: Thread, cx: &mut Context<Self>) {
        let same_host = self
            .host
            .as_ref()
            .is_some_and(|h| Arc::ptr_eq(h, &session.host))
            && self.repo.as_ref() == Some(&session.repo);
        if !same_host {
            self.host = Some(session.host.clone());
            self.repo = Some(session.repo.clone());
            self.threads.clear();
            self.order.clear();
            self.current = None;
        }
        if self.current == Some(thread) {
            return;
        }
        self.current = Some(thread);
        self.order.retain(|t| *t != thread);
        self.order.push_back(thread);
        while self.order.len() > KEPT {
            if let Some(old) = self.order.pop_front() {
                self.threads.remove(&old);
                if cx.has_global::<ThreadPeople>() {
                    let g = cx.global_mut::<ThreadPeople>();
                    g.seen.remove(&old);
                    g.head.remove(&old);
                }
            }
        }
        self.reload(cx);
    }

    /// Nothing open (back to the list); the loaded conversations stay for re-opening.
    pub fn hide(&mut self) {
        self.current = None;
    }

    /// Asks the host again for every section of the open thread (after a refresh or a post).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.current else {
            return;
        };
        self.start::<TimelineItem>(thread, cx);
        if let Thread::PullRequest(_) = thread {
            self.start::<Review>(thread, cx);
            self.start::<ReviewComment>(thread, cx);
        }
        self.publish(thread, cx);
        cx.notify();
    }

    /// A comment was posted on `thread`: shown at once, then the timeline is fetched again
    /// (the post dropped the host's cache, so this is a real request).
    pub fn posted(&mut self, thread: Thread, comment: ThreadComment, cx: &mut Context<Self>) {
        if let Some(state) = self.threads.get_mut(&thread) {
            state.timeline.push(TimelineItem::Comment(comment));
            if self.current == Some(thread) {
                self.start::<TimelineItem>(thread, cx);
            }
            self.publish(thread, cx);
        }
        cx.notify();
    }

    /// Hands the participants of `thread` to the page's side column and asks for their
    /// images (the first [`FACES`]; already known ones cost nothing).
    fn publish(&mut self, thread: Thread, cx: &mut Context<Self>) {
        let people = self
            .threads
            .get(&thread)
            .map(ThreadState::people)
            .unwrap_or_default();
        if !self.watching_avatars
            && let Some(avatars) = crate::avatars::handle(cx)
        {
            self.watching_avatars = true;
            cx.observe(&avatars, |_, _, cx| cx.notify()).detach();
        }
        request_faces(&people, cx);
        if !cx.has_global::<ThreadPeople>() {
            cx.set_global(ThreadPeople::default());
        }
        let known = &mut cx.global_mut::<ThreadPeople>().seen;
        if known.get(&thread) != Some(&people) {
            known.insert(thread, people);
        }
    }

    fn start<T: Part>(&mut self, thread: Thread, cx: &mut Context<Self>) {
        let supported = self.host.as_ref().is_some_and(|h| h.supports(T::FEATURE));
        let section = T::section(self.threads.entry(thread).or_default());
        if !supported {
            section.unsupported = true;
            return;
        }
        let generation = section.begin();
        self.request::<T>(thread, generation, vec![1], cx);
    }

    /// One task per page: sections and pages load side by side.
    fn request<T: Part>(
        &mut self,
        thread: Thread,
        generation: u64,
        pages: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        let (Some(host), Some(repo)) = (&self.host, &self.repo) else {
            return;
        };
        for page in pages {
            let answer = T::fetch(host.clone(), repo.clone(), thread, page);
            cx.spawn(async move |this, cx| {
                let result = answer.await;
                _ = this.update(cx, |this, cx| {
                    let Some(state) = this.threads.get_mut(&thread) else {
                        return;
                    };
                    let next = T::section(state).arrived(generation, page, result);
                    this.request::<T>(thread, generation, next, cx);
                    this.publish(thread, cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn show_more<T: Part>(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.current else {
            return;
        };
        let Some(state) = self.threads.get_mut(&thread) else {
            return;
        };
        let section = T::section(state);
        let generation = section.generation;
        let pages = section.show_more();
        self.request::<T>(thread, generation, pages, cx);
        cx.notify();
    }

    fn retry<T: Part>(&mut self, cx: &mut Context<Self>) {
        if let Some(thread) = self.current {
            self.start::<T>(thread, cx);
            cx.notify();
        }
    }

    fn web_url(&self, thread: Thread) -> Option<String> {
        let target = match thread {
            Thread::Issue(n) => WebTarget::Issue(n),
            Thread::PullRequest(n) => WebTarget::PullRequest(n),
        };
        self.host.as_ref()?.web_url(self.repo.as_ref()?, &target)
    }
}

impl Default for ConversationView {
    fn default() -> Self {
        Self::new()
    }
}

// ---- words --------------------------------------------------------------------------------

/// The sentence of an activity line: `octo added label bug`. Azure's system lines have no
/// actor and are a sentence already.
pub fn event_text(event: &TimelineEvent) -> String {
    let verb = match &event.kind {
        EventKind::Labeled => "added label",
        EventKind::Unlabeled => "removed label",
        EventKind::Assigned => "assigned",
        EventKind::Unassigned => "unassigned",
        EventKind::Closed => "closed this",
        EventKind::Reopened => "reopened this",
        EventKind::Merged => "merged this",
        EventKind::Renamed => "changed the title",
        EventKind::Referenced => "referenced this",
        EventKind::Committed => "committed",
        EventKind::ForcePushed => "force-pushed the branch",
        EventKind::ReviewRequested => "requested a review from",
        EventKind::ReviewRequestRemoved => "removed the review request for",
        EventKind::Reviewed => "reviewed:",
        EventKind::Milestoned => "added this to milestone",
        EventKind::Demilestoned => "removed this from milestone",
        EventKind::Locked => "locked the conversation",
        EventKind::Unlocked => "unlocked the conversation",
        EventKind::ReadyForReview => "marked this ready for review",
        EventKind::ConvertedToDraft => "marked this as draft",
        EventKind::BranchDeleted => "deleted the branch",
        EventKind::Other(what) => what.as_str(),
    };
    let actor = event.actor.as_ref().map(|a| a.login.as_str());
    match (actor, event.detail.as_deref()) {
        (Some(actor), Some(detail)) => format!("{actor} {verb} {detail}"),
        (Some(actor), None) => format!("{actor} {verb}"),
        (None, Some(detail)) => detail.to_string(),
        (None, None) => verb.to_string(),
    }
}

fn event_icon(kind: &EventKind) -> IconName {
    match kind {
        EventKind::Labeled | EventKind::Unlabeled => IconName::Tag,
        EventKind::Assigned
        | EventKind::Unassigned
        | EventKind::ReviewRequested
        | EventKind::ReviewRequestRemoved => IconName::User,
        EventKind::Closed => IconName::CircleX,
        EventKind::Merged => IconName::GitMerge,
        EventKind::Renamed => IconName::Pencil,
        EventKind::Referenced => IconName::Link,
        EventKind::Committed | EventKind::ForcePushed => IconName::GitCommitHorizontal,
        EventKind::Reviewed => IconName::Eye,
        EventKind::Milestoned | EventKind::Demilestoned => IconName::Milestone,
        EventKind::Locked => IconName::Lock,
        EventKind::Unlocked => IconName::LockOpen,
        EventKind::ReadyForReview | EventKind::ConvertedToDraft => IconName::GitPullRequest,
        EventKind::BranchDeleted => IconName::GitBranch,
        EventKind::Reopened | EventKind::Other(_) => IconName::CircleDot,
    }
}

pub fn review_word(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "Approved",
        ReviewState::ChangesRequested => "Changes requested",
        ReviewState::Commented => "Commented",
        ReviewState::Dismissed => "Dismissed",
        ReviewState::Pending => "Pending",
    }
}

fn review_color(state: ReviewState, palette: &Palette) -> Hsla {
    match state {
        ReviewState::Approved => palette.ok,
        ReviewState::ChangesRequested => palette.error,
        ReviewState::Pending => palette.warn,
        ReviewState::Commented => palette.muted,
        ReviewState::Dismissed => palette.border_strong,
    }
}

/// Reviews worth a row: a bare "commented" review is only the wrapper of inline comments.
fn shown_reviews<'a>(reviews: impl Iterator<Item = &'a Review>) -> Vec<&'a Review> {
    reviews
        .filter(|r| r.state != ReviewState::Commented || r.body.is_some())
        .collect()
}

/// Inline comments under their `(path, line)`.
type Groups<'a> = Vec<((String, Option<u32>), Vec<&'a ReviewComment>)>;

/// Inline comments by `(path, line)` in first-comment order, replies after their root.
pub fn group_review_comments<'a>(comments: impl Iterator<Item = &'a ReviewComment>) -> Groups<'a> {
    let mut groups: Groups<'a> = Vec::new();
    let mut root_group: HashMap<u64, usize> = HashMap::new();
    for c in comments {
        let ix = c
            .in_reply_to
            .and_then(|root| root_group.get(&root).copied())
            .or_else(|| {
                let key = (c.path.clone(), c.line.or(c.original_line));
                groups.iter().position(|(k, _)| *k == key)
            });
        let ix = match ix {
            Some(ix) => ix,
            None => {
                groups.push(((c.path.clone(), c.line.or(c.original_line)), Vec::new()));
                groups.len() - 1
            }
        };
        root_group.insert(c.id, ix);
        groups[ix].1.push(c);
    }
    groups
}

fn capped(text: &str) -> String {
    text.trim().chars().take(MAX_BODY).collect()
}

/// A comment body as Markdown (or HTML for Azure work items).
pub fn body_view(id: impl Into<ElementId>, body: &str, format: BodyFormat) -> AnyElement {
    let text = SharedString::from(capped(body));
    let view = match format {
        BodyFormat::Markdown => TextView::markdown(id, text),
        BodyFormat::Html => TextView::html(id, text),
    };
    div()
        .w_full()
        .text_sm()
        .child(view.selectable(true))
        .into_any_element()
}

// ---- rendering ----------------------------------------------------------------------------

impl Render for ConversationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let Some(thread) = self.current else {
            return div().into_any_element();
        };
        let web = self.web_url(thread);
        let now = logic::now_secs();
        let Some(state) = self.threads.get(&thread) else {
            return div().into_any_element();
        };
        let mut sections: Vec<AnyElement> = Vec::new();
        if let Thread::PullRequest(_) = thread
            && !state.reviews.unsupported
        {
            sections.push(self.render_reviews(&state.reviews, now, &palette, cx));
        }
        if !state.timeline.unsupported {
            sections.push(self.render_timeline(&state.timeline, now, &palette, cx));
        }
        if let Thread::PullRequest(_) = thread
            && !state.comments.unsupported
        {
            sections.push(self.render_inline(&state.comments, now, &palette, cx));
        }
        let more = [state.timeline.more, state.reviews.more, state.comments.more];
        v_flex()
            .w_full()
            .gap_4()
            .children(sections)
            .when(more.iter().any(|m| *m) && web.is_some(), |d| {
                let url = web.clone().unwrap_or_default();
                d.child(
                    h_flex().justify_center().child(
                        Button::new("conv-open-web")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .label("Open the whole conversation on the web")
                            .on_click(move |_: &ClickEvent, window, cx| {
                                crate::links::open(&url, Some(window), cx)
                            }),
                    ),
                )
            })
            .into_any_element()
    }
}

/// A conversation card: inset one step from the page (`panel`) with a border and rounded corners.
fn card_shell(palette: &Palette) -> gpui_kit::Div {
    v_flex()
        .w_full()
        .rounded(px(8.))
        .border_1()
        .border_color(palette.border)
        .bg(palette.bg)
        .overflow_hidden()
}

/// The strip across the top of a card: author line, pills.
fn card_header(palette: &Palette) -> gpui_kit::Div {
    h_flex()
        .gap_2()
        .px_3()
        .py_1p5()
        .bg(palette.panel2)
        .border_b_1()
        .border_color(palette.border)
}

fn card_body() -> gpui_kit::Div {
    v_flex().gap_1().px_3().py_2()
}

impl ConversationView {
    /// Title row with a spinner while anything of the section is on its way.
    fn header(title: String, busy: bool, palette: &Palette, cx: &App) -> AnyElement {
        h_flex()
            .gap_2()
            .pb_1()
            .border_b_1()
            .border_color(palette.border)
            .child(ui::typography::section_title(title, cx))
            .when(busy, |d| {
                d.child(Spinner::new().xsmall().color(palette.muted))
            })
            .into_any_element()
    }

    /// The skeleton, the inline error, "Loading more" and "Show more" around a section's rows.
    fn frame<T: Part>(
        section: &Section<T>,
        id: &'static str,
        what: &str,
        rows: Vec<AnyElement>,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(error) = &section.error {
            let this = cx.entity().downgrade();
            out.push(error_banner(error, palette, id, move |_, cx| {
                _ = this.update(cx, |view, cx| view.retry::<T>(cx));
            }));
        }
        if section.loading_first() {
            out.push(
                v_flex()
                    .gap_2()
                    .child(ui::typography::caption(
                        format!("Loading {what}\u{2026}"),
                        cx,
                    ))
                    .child(Skeleton::new().w_3_4().h(px(12.)))
                    .child(Skeleton::new().secondary().w_full().h(px(12.)))
                    .child(Skeleton::new().secondary().w_1_2().h(px(12.)))
                    .into_any_element(),
            );
            return out;
        }
        if section.loaded && rows.is_empty() && section.error.is_none() {
            out.push(ui::typography::caption(format!("No {what}."), cx).into_any_element());
        }
        out.extend(rows);
        if section.loading_more() {
            out.push(
                h_flex()
                    .gap_2()
                    .child(Spinner::new().xsmall().color(palette.muted))
                    .child(ui::typography::caption("Loading more\u{2026}", cx))
                    .into_any_element(),
            );
        } else if section.more {
            out.push(
                h_flex()
                    .gap_2()
                    .child(ui::typography::caption(
                        format!("Showing the first {}.", section.len()),
                        cx,
                    ))
                    .child(
                        Button::new(SharedString::from(format!("{id}-more")))
                            .xsmall()
                            .outline()
                            .label("Show more")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.show_more::<T>(cx)),
                            ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn author_line(
        login: Option<&str>,
        when: &str,
        extra: Option<&str>,
        now: i64,
        palette: &Palette,
    ) -> AnyElement {
        h_flex()
            .gap_1p5()
            .text_xs()
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(palette.emph)
                    .child(SharedString::from(
                        ui::sanitize_line(login.unwrap_or("unknown")).into_owned(),
                    )),
            )
            .when(!when.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(palette.muted)
                        .child(SharedString::from(logic::ago(when, now))),
                )
            })
            .children(extra.map(|e| {
                div()
                    .text_color(palette.muted)
                    .child(SharedString::from(e.to_string()))
            }))
            .into_any_element()
    }

    fn render_timeline(
        &self,
        section: &Section<TimelineItem>,
        now: i64,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let comments = section
            .items()
            .filter(|i| matches!(i, TimelineItem::Comment(_)))
            .count();
        let rows: Vec<AnyElement> = section
            .items()
            .enumerate()
            .map(|(ix, item)| match item {
                TimelineItem::Comment(c) => card_shell(palette)
                    .child(card_header(palette).child(Self::author_line(
                        c.author.as_ref().map(|a| a.login.as_str()),
                        &c.created_at,
                        c.edited.then_some("\u{b7} edited"),
                        now,
                        palette,
                    )))
                    .child(card_body().child(body_view(
                        ElementId::NamedInteger("conv-comment".into(), ix as u64),
                        &c.body,
                        c.format,
                    )))
                    .into_any_element(),
                TimelineItem::Event(e) => h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(
                        Icon::new(event_icon(&e.kind))
                            .xsmall()
                            .text_color(palette.muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(palette.fg)
                            .child(SharedString::from(
                                ui::sanitize_line(&event_text(e)).into_owned(),
                            )),
                    )
                    .when(!e.created_at.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .child(SharedString::from(logic::ago(&e.created_at, now))),
                        )
                    })
                    .into_any_element(),
            })
            .collect();
        let title = if section.loaded {
            format!("CONVERSATION \u{b7} {comments}")
        } else {
            "CONVERSATION".to_string()
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(Self::header(title, section.busy(), palette, cx))
            .children(Self::frame(
                section,
                "conv-timeline",
                "comments",
                rows,
                palette,
                cx,
            ))
            .into_any_element()
    }

    fn render_reviews(
        &self,
        section: &Section<Review>,
        now: i64,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows: Vec<AnyElement> = shown_reviews(section.items())
            .into_iter()
            .enumerate()
            .map(|(ix, r)| {
                let color = review_color(r.state, palette);
                card_shell(palette)
                    .child(
                        card_header(palette)
                            .child(super::pill(review_word(r.state), color, palette))
                            .child(Self::author_line(
                                r.author.as_ref().map(|a| a.login.as_str()),
                                r.submitted_at.as_deref().unwrap_or_default(),
                                None,
                                now,
                                palette,
                            )),
                    )
                    .children(r.body.as_deref().map(|body| {
                        card_body().child(body_view(
                            ElementId::NamedInteger("conv-review".into(), ix as u64),
                            body,
                            BodyFormat::Markdown,
                        ))
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .w_full()
            .gap_2()
            .child(Self::header("REVIEWS".into(), section.busy(), palette, cx))
            .children(Self::frame(
                section,
                "conv-reviews",
                "reviews",
                rows,
                palette,
                cx,
            ))
            .into_any_element()
    }

    fn render_inline(
        &self,
        section: &Section<ReviewComment>,
        now: i64,
        palette: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let groups = group_review_comments(section.items());
        let mut n = 0u64;
        let rows: Vec<AnyElement> = groups
            .into_iter()
            .map(|((path, line), comments)| {
                let side = comments.first().and_then(|c| c.side);
                let outdated = comments.first().is_some_and(|c| c.line.is_none());
                let location = match (line, side) {
                    (Some(line), Some(DiffSide::Left)) => format!("{path}:{line} (old)"),
                    (Some(line), _) => format!("{path}:{line}"),
                    (None, _) => path.clone(),
                };
                let hunk = comments
                    .first()
                    .and_then(|c| c.diff_hunk.as_deref())
                    .map(|h| {
                        let lines: Vec<&str> = h.lines().collect();
                        lines[lines.len().saturating_sub(HUNK_LINES)..].join("\n")
                    });
                card_shell(palette)
                    .child(
                        card_header(palette)
                            .child(
                                ui::typography::mono(ui::sanitize_line(&location).into_owned(), cx)
                                    .flex_1()
                                    .min_w_0()
                                    .truncate(),
                            )
                            .when(outdated, |d| {
                                d.child(super::pill("Outdated", palette.warn, palette))
                            }),
                    )
                    .children(hunk.map(|hunk| {
                        v_flex()
                            .px_2()
                            .py_1()
                            .bg(palette.panel)
                            .children(hunk.lines().map(|l| {
                                let color = if l.starts_with('+') {
                                    palette.ok
                                } else if l.starts_with('-') {
                                    palette.error
                                } else {
                                    palette.muted
                                };
                                ui::typography::mono(
                                    SharedString::from(ui::sanitize_line(l).into_owned()),
                                    cx,
                                )
                                .text_color(color)
                                .whitespace_nowrap()
                                .overflow_hidden()
                            }))
                    }))
                    .children(comments.into_iter().map(|c| {
                        n += 1;
                        v_flex()
                            .gap_1()
                            .px_2()
                            .py_1p5()
                            .border_t_1()
                            .border_color(palette.border)
                            .when(c.in_reply_to.is_some(), |d| d.pl_5())
                            .child(Self::author_line(
                                c.author.as_ref().map(|a| a.login.as_str()),
                                &c.created_at,
                                None,
                                now,
                                palette,
                            ))
                            .child(body_view(
                                ElementId::NamedInteger("conv-inline".into(), n),
                                &c.body,
                                BodyFormat::Markdown,
                            ))
                    }))
                    .into_any_element()
            })
            .collect();
        let title = if section.loaded {
            format!("INLINE COMMENTS \u{b7} {}", section.len())
        } else {
            "INLINE COMMENTS".to_string()
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(Self::header(title, section.busy(), palette, cx))
            .children(Self::frame(
                section,
                "conv-inline",
                "inline comments",
                rows,
                palette,
                cx,
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(items: Vec<u32>, next: Option<u32>, last: Option<u32>) -> HostResult<Page<u32>> {
        Ok(Page { items, next, last })
    }

    #[test]
    fn parallel_pages_when_the_last_is_known() {
        let mut s = Section::<u32>::default();
        let g = s.begin();
        assert!(s.loading_first());
        let next = s.arrived(g, 1, page(vec![1], Some(2), Some(4)));
        assert_eq!(next, [2, 3, 4]);
        assert!(s.loaded && s.loading_more());
        // Out of order is fine; items stay in page order.
        assert!(s.arrived(g, 3, page(vec![3], Some(4), Some(4))).is_empty());
        assert!(s.arrived(g, 2, page(vec![2], Some(3), Some(4))).is_empty());
        assert!(s.arrived(g, 4, page(vec![4], None, None)).is_empty());
        assert_eq!(s.items().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert!(!s.busy() && !s.more);
    }

    #[test]
    fn sequential_pages_without_a_last() {
        let mut s = Section::<u32>::default();
        let g = s.begin();
        assert_eq!(s.arrived(g, 1, page(vec![1], Some(2), None)), [2]);
        assert_eq!(s.arrived(g, 2, page(vec![2], Some(3), None)), [3]);
        assert!(s.arrived(g, 3, page(vec![3], None, None)).is_empty());
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn capped_then_show_more() {
        let mut s = Section::<u32>::default();
        let g = s.begin();
        let next = s.arrived(g, 1, page(vec![1], Some(2), Some(25)));
        assert_eq!(next, (2..=MAX_PAGES).collect::<Vec<_>>());
        assert!(s.more);
        for p in next {
            s.arrived(g, p, page(vec![p], Some(p + 1), Some(25)));
        }
        assert_eq!(s.show_more(), (11..=20).collect::<Vec<_>>());
        assert!(s.more);

        // Sequential: the chain stops at the cap.
        let mut s = Section::<u32>::default();
        let g = s.begin();
        let mut p = 1;
        loop {
            let next = s.arrived(g, p, page(vec![p], Some(p + 1), None));
            match next.as_slice() {
                [n] => p = *n,
                _ => break,
            }
        }
        assert_eq!(p, MAX_PAGES);
        assert!(s.more);
        assert_eq!(s.show_more(), [MAX_PAGES + 1]);
    }

    #[test]
    fn revalidation_keeps_old_pages_and_drops_stale_answers() {
        let mut s = Section::<u32>::default();
        let g1 = s.begin();
        assert_eq!(s.arrived(g1, 1, page(vec![1], Some(2), None)), [2]);
        let g2 = s.begin();
        assert!(!s.loading_first(), "old data stays on screen");
        assert!(s.arrived(g1, 2, page(vec![2], None, None)).is_empty());
        assert_eq!(s.len(), 1, "the old round's answer is ignored");
        assert!(s.arrived(g2, 1, page(vec![9], None, None)).is_empty());
        assert_eq!(s.items().copied().collect::<Vec<_>>(), [9]);
    }

    #[test]
    fn errors_and_unsupported() {
        let mut s = Section::<u32>::default();
        let g = s.begin();
        let err = HostError::RateLimited {
            host: "github.com".into(),
            until: None,
        };
        assert!(s.arrived(g, 1, Err(err)).is_empty());
        assert!(s.error.is_some() && !s.loaded);
        let g = s.begin();
        assert!(s.error.is_none());
        s.arrived(g, 1, Err(HostError::Unsupported(Feature::Reviews)));
        assert!(s.unsupported);
    }

    #[test]
    fn participants_dedupe_by_login_in_order() {
        let p = |login: &str, avatar: Option<&str>| Person {
            login: login.into(),
            name: None,
            avatar_url: avatar.map(Into::into),
            web_url: None,
        };
        let all = [
            p("Ann", None),
            p("bob", None),
            p("ann", Some("u")),
            p("", None),
        ];
        let got = participants(all.iter());
        let logins: Vec<&str> = got.iter().map(|p| p.login.as_str()).collect();
        assert_eq!(logins, ["Ann", "bob"]);
        assert_eq!(got[0].avatar_url.as_deref(), Some("u"));
    }

    #[test]
    fn avatar_keys_are_github_only() {
        let mut who = Person {
            login: "octo".into(),
            name: None,
            avatar_url: Some("https://avatars.githubusercontent.com/u/42?v=4".into()),
            web_url: None,
        };
        assert_eq!(
            avatar_key(&who).as_deref(),
            Some("42+octo@users.noreply.github.com")
        );
        who.avatar_url = None;
        assert_eq!(avatar_key(&who), None);
        who.web_url = Some("https://github.com/octo".into());
        assert_eq!(
            avatar_key(&who).as_deref(),
            Some("octo@users.noreply.github.com")
        );
    }

    fn person(login: &str) -> Option<Person> {
        Some(Person {
            login: login.into(),
            name: None,
            avatar_url: None,
            web_url: None,
        })
    }

    #[test]
    fn event_sentences() {
        let ev = |kind, actor: Option<&str>, detail: Option<&str>| TimelineEvent {
            kind,
            actor: actor.and_then(person),
            created_at: String::new(),
            detail: detail.map(str::to_string),
        };
        assert_eq!(
            event_text(&ev(EventKind::Labeled, Some("octo"), Some("bug"))),
            "octo added label bug"
        );
        assert_eq!(
            event_text(&ev(EventKind::Closed, Some("octo"), None)),
            "octo closed this"
        );
        assert_eq!(
            event_text(&ev(EventKind::Reviewed, None, Some("Ana voted 10"))),
            "Ana voted 10"
        );
        assert_eq!(
            event_text(&ev(EventKind::Other("connected".into()), Some("o"), None)),
            "o connected"
        );
    }

    #[test]
    fn inline_comments_group_by_line_with_replies() {
        let c = |id, path: &str, line, reply| ReviewComment {
            id,
            author: person("a"),
            path: path.into(),
            line,
            original_line: line,
            side: None,
            body: String::new(),
            diff_hunk: None,
            in_reply_to: reply,
            created_at: String::new(),
            web_url: None,
        };
        let all = [
            c(1, "a.rs", Some(3), None),
            c(2, "b.rs", Some(1), None),
            c(3, "a.rs", Some(9), Some(1)),
            c(4, "a.rs", Some(3), None),
        ];
        let groups = group_review_comments(all.iter());
        let ids: Vec<Vec<u64>> = groups
            .iter()
            .map(|(_, cs)| cs.iter().map(|c| c.id).collect())
            .collect();
        assert_eq!(ids, [vec![1, 3, 4], vec![2]]);
        let reviews = [
            Review {
                id: 1,
                author: None,
                state: ReviewState::Commented,
                body: None,
                submitted_at: None,
                web_url: None,
            },
            Review {
                id: 2,
                author: None,
                state: ReviewState::Approved,
                body: None,
                submitted_at: None,
                web_url: None,
            },
        ];
        assert_eq!(shown_reviews(reviews.iter()).len(), 1);
    }
}
