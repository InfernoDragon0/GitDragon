//! Author avatars from free sources, cached in memory and on disk: GitHub's noreply emails
//! carry the user id (no API call), and Gravatar (opt-in: it sends a hash of the email to a
//! third party). Authors those do not know are asked of the repo's hosting service through the
//! `GitHost` trait (see `hosted`). Until an image is in, or when there is none, views draw
//! coloured initials.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Image, ImageFormat, Task,
};
use sha2::{Digest as _, Sha256};

use crate::hosted::{HostedRepo, Lookup};

/// Disk entries older than this are fetched again.
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Avatars are small; anything bigger is not one.
const MAX_BYTES: usize = 512 * 1024;
/// Fetched per round; the rest wait for the next.
const BATCH: usize = 12;

pub struct Avatars {
    /// `<config>/avatars`; `None` keeps everything in memory.
    dir: Option<PathBuf>,
    gravatar: bool,
    /// Lowercased email -> image (`None`: no avatar).
    images: HashMap<String, Option<Arc<Image>>>,
    queued: Vec<String>,
    queued_set: HashSet<String>,
    /// Lowercased email -> the repo's host and a commit by that email, for emails the free
    /// sources cannot place.
    hints: HashMap<String, (HostedRepo, String)>,
    /// `url:` keys (see [`url_key`]) -> the https URL they stand for.
    urls: HashMap<String, String>,
    /// Emails to leave alone until then (host busy, commit not pushed, no account).
    later: HashMap<String, Instant>,
    task: Option<Task<()>>,
}

/// How a queued email came out.
enum Outcome {
    /// Settled: an image, or none.
    Done(Option<Arc<Image>>),
    /// Not settled: ask again after this long.
    Later(Duration),
}

/// The Gravatar setting changed (the workspace saves it).
pub struct SettingChanged;

impl EventEmitter<SettingChanged> for Avatars {}

struct AvatarsGlobal(Entity<Avatars>);

impl Global for AvatarsGlobal {}

pub fn init(gravatar: bool, cx: &mut App) {
    let dir = crate::state::config_dir().map(|d| d.join("avatars"));
    let avatars = cx.new(|_| Avatars {
        dir,
        gravatar,
        images: HashMap::new(),
        queued: Vec::new(),
        queued_set: HashSet::new(),
        hints: HashMap::new(),
        urls: HashMap::new(),
        later: HashMap::new(),
        task: None,
    });
    cx.set_global(AvatarsGlobal(avatars));
}

pub fn handle(cx: &App) -> Option<Entity<Avatars>> {
    cx.try_global::<AvatarsGlobal>().map(|g| g.0.clone())
}

/// The avatar for `email` if it is loaded (no fetching: see [`Avatars::request`]).
pub fn image(email: &str, cx: &App) -> Option<Arc<Image>> {
    let avatars = handle(cx)?;
    avatars
        .read(cx)
        .images
        .get(&email.trim().to_ascii_lowercase())
        .cloned()
        .flatten()
}

impl Avatars {
    pub fn gravatar(&self) -> bool {
        self.gravatar
    }

    /// Turns Gravatar lookups on or off; emails without an avatar are asked again.
    pub fn set_gravatar(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.gravatar != on {
            self.gravatar = on;
            self.images.retain(|_, image| image.is_some());
            cx.emit(SettingChanged);
            cx.notify();
        }
    }

    /// Whether asking for `email` could still change anything (not settled, not put off).
    pub fn pending(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        !email.is_empty()
            && !self.images.contains_key(&email)
            && !self.later.get(&email).is_some_and(|t| *t > Instant::now())
    }

    /// Like [`Avatars::request`] for `(email, commit)` authors of `repo`: the free sources go
    /// first, then the repo's host is asked about the commit.
    pub fn request_hosted<'a>(
        &mut self,
        repo: &HostedRepo,
        authors: impl IntoIterator<Item = (&'a str, &'a str)>,
        cx: &mut Context<Self>,
    ) {
        let authors: Vec<(&str, &str)> = authors.into_iter().collect();
        for &(email, commit) in &authors {
            let email = email.trim().to_ascii_lowercase();
            if !email.is_empty() && !self.images.contains_key(&email) {
                self.hints.insert(email, (repo.clone(), commit.to_owned()));
            }
        }
        self.request(authors.into_iter().map(|(email, _)| email), cx);
    }

    /// Queues a picture the host named by URL (https only, like every fetch here); the image
    /// is then found with [`image`] under the returned key, which never collides with an email.
    /// Same disk cache, size cap and batching as emails.
    pub fn request_url(&mut self, url: &str, cx: &mut Context<Self>) -> Option<String> {
        let key = url_key(url)?;
        self.urls
            .entry(key.clone())
            .or_insert_with(|| url.trim().to_owned());
        self.request([key.as_str()], cx);
        Some(key)
    }

    /// Queues the emails not known yet; images arrive in the background (observers are
    /// notified).
    pub fn request<'a>(
        &mut self,
        emails: impl IntoIterator<Item = &'a str>,
        cx: &mut Context<Self>,
    ) {
        for email in emails {
            let email = email.trim().to_ascii_lowercase();
            if email.is_empty()
                || self.images.contains_key(&email)
                || self.queued_set.contains(&email)
                || self.later.get(&email).is_some_and(|t| *t > Instant::now())
            {
                continue;
            }
            self.queued_set.insert(email.clone());
            self.queued.push(email);
        }
        self.pump(cx);
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.task.is_some() || self.queued.is_empty() {
            return;
        }
        let take = self.queued.len().min(BATCH);
        let batch: Vec<String> = self.queued.drain(..take).collect();
        let jobs: Vec<Job> = batch
            .into_iter()
            .map(|email| {
                let url = self
                    .urls
                    .get(&email)
                    .cloned()
                    .or_else(|| avatar_url(&email, self.gravatar));
                // GitHub's own noreply addresses are fully answered by the free source.
                let hint = self.hints.remove(&email).filter(|_| !is_noreply(&email));
                (email, url, hint)
            })
            .collect();
        let dir = self.dir.clone();
        let work = cx.background_executor().spawn(async move {
            let mut found = Vec::new();
            for (email, url, hint) in jobs {
                let mut bytes = match url {
                    Some(url) => load(dir.as_deref(), &url).await,
                    None => None,
                };
                let mut later = None;
                if bytes.is_none()
                    && let Some((repo, commit)) = hint
                {
                    match hosted_url(dir.as_deref(), &repo, &commit, &email).await {
                        // HTTPS only; `fetch_public` insists on it too.
                        Lookup::Url(url) if url.starts_with("https://") => {
                            bytes = load(dir.as_deref(), &url).await;
                        }
                        Lookup::Later(wait) => later = Some(wait),
                        Lookup::Url(_) | Lookup::Miss => {}
                    }
                }
                let outcome = match later {
                    Some(wait) if bytes.is_none() => Outcome::Later(wait),
                    _ => Outcome::Done(bytes.and_then(decode)),
                };
                found.push((email, outcome));
            }
            found
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let found = work.await;
            _ = this.update(cx, |this, cx| {
                this.task = None;
                let mut settled = false;
                for (email, outcome) in found {
                    this.queued_set.remove(&email);
                    match outcome {
                        Outcome::Done(image) => {
                            this.images.insert(email, image);
                            settled = true;
                        }
                        Outcome::Later(wait) => {
                            this.later.insert(email, Instant::now() + wait);
                        }
                    }
                }
                // Views ask on every render: only a change may cause the next one.
                if settled {
                    cx.notify();
                }
                this.pump(cx);
            });
        }));
    }
}

/// A queued email with its free URL and the host to ask when that has nothing.
type Job = (String, Option<String>, Option<(HostedRepo, String)>);

/// The key of a picture at `url` for [`image`] and [`Avatars::request_url`]: `url:` and the
/// hash of the URL. `None` unless it is a plain https URL of sane length.
pub fn url_key(url: &str) -> Option<String> {
    let url = url.trim();
    (url.starts_with("https://")
        && url.len() <= 2048
        && !url.bytes().any(|b| b.is_ascii_control() || b == b' '))
    .then(|| format!("url:{}", hex_sha256(url)))
}

fn is_noreply(email: &str) -> bool {
    email.ends_with("@users.noreply.github.com")
}

/// Where a free avatar for `email` lives, if anywhere.
fn avatar_url(email: &str, gravatar: bool) -> Option<String> {
    if let Some(local) = email.strip_suffix("@users.noreply.github.com") {
        // `12345+login@…` (since 2017) carries the user id; `login@…` only the login.
        if let Some((id, _)) = local.split_once('+')
            && !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(format!(
                "https://avatars.githubusercontent.com/u/{id}?s=64&v=4"
            ));
        }
        if !local.is_empty()
            && local
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Some(format!("https://github.com/{local}.png?size=64"));
        }
        return None;
    }
    gravatar.then(|| {
        format!(
            "https://gravatar.com/avatar/{}?s=64&d=404",
            hex_sha256(email)
        )
    })
}

fn hex_sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn fresh(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < MAX_AGE)
}

/// The host's avatar URL for `email`: from the disk cache when fresh (`<key>.host` holds the
/// URL, empty for "none"; keyed by service and email, so the API is not asked again every
/// launch), else the host is asked. Only settled answers are cached.
async fn hosted_url(
    dir: Option<&std::path::Path>,
    repo: &HostedRepo,
    commit: &str,
    email: &str,
) -> Lookup {
    let file = dir
        .zip(repo.scope())
        .map(|(dir, scope)| dir.join(format!("{}.host", hex_sha256(&format!("{scope}\0{email}")))));
    if let Some(file) = &file
        && fresh(file)
        && let Ok(text) = std::fs::read_to_string(file)
    {
        let text = text.trim();
        return if text.is_empty() {
            Lookup::Miss
        } else {
            Lookup::Url(text.to_owned())
        };
    }
    let found = repo.avatar(commit, email).await;
    if let Some(file) = &file
        && let Some(dir) = file.parent()
        && crate::state::create_private_dir(dir).is_ok()
    {
        _ = match &found {
            Lookup::Url(url) => crate::state::write_private(file, url),
            Lookup::Miss => crate::state::write_private(file, b""),
            Lookup::Later(_) => Ok(()),
        };
    }
    found
}

/// From the disk cache when fresh, else fetched (and cached, including "none").
async fn load(dir: Option<&std::path::Path>, url: &str) -> Option<Vec<u8>> {
    let key = hex_sha256(url);
    if let Some(dir) = dir {
        let image = dir.join(format!("{key}.img"));
        if fresh(&image) {
            return std::fs::read(&image).ok();
        }
        if fresh(&dir.join(format!("{key}.none"))) {
            return None;
        }
    }
    let url = url.to_string();
    let fetched = accounts::net::spawn(accounts::net::fetch_public(url, MAX_BYTES))
        .await
        .ok()?;
    let Ok(fetched) = fetched else {
        // Offline or a server error: try again next launch, cache nothing.
        return None;
    };
    if let Some(dir) = dir
        && crate::state::create_private_dir(dir).is_ok()
    {
        _ = match &fetched {
            Some(bytes) => crate::state::write_private(&dir.join(format!("{key}.img")), bytes),
            None => crate::state::write_private(&dir.join(format!("{key}.none")), b""),
        };
    }
    fetched
}

/// An image GPUI can draw, by its magic bytes.
fn decode(bytes: Vec<u8>) -> Option<Arc<Image>> {
    let format = match bytes.as_slice() {
        [0x89, b'P', b'N', b'G', ..] => ImageFormat::Png,
        [0xff, 0xd8, ..] => ImageFormat::Jpeg,
        [b'G', b'I', b'F', ..] => ImageFormat::Gif,
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => ImageFormat::Webp,
        _ => return None,
    };
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            avatar_url("12345+octo@users.noreply.github.com", false).as_deref(),
            Some("https://avatars.githubusercontent.com/u/12345?s=64&v=4")
        );
        assert_eq!(
            avatar_url("octo-cat@users.noreply.github.com", false).as_deref(),
            Some("https://github.com/octo-cat.png?size=64")
        );
        assert_eq!(avatar_url("bad/x@users.noreply.github.com", true), None);
        assert_eq!(avatar_url("me@example.com", false), None);
        let gravatar = avatar_url("me@example.com", true).unwrap();
        assert!(gravatar.starts_with("https://gravatar.com/avatar/"));
        assert!(gravatar.ends_with("?s=64&d=404"));
        assert_eq!(
            gravatar.len(),
            "https://gravatar.com/avatar/".len() + 64 + 11
        );
    }

    #[test]
    fn url_keys() {
        let key = url_key("https://example.com/a.png?s=64").unwrap();
        assert!(key.starts_with("url:") && !key.contains('@'));
        assert_eq!(url_key(" https://example.com/a.png?s=64 "), Some(key));
        assert_eq!(url_key("http://example.com/a.png"), None);
        assert_eq!(url_key("https://exa mple.com/a.png"), None);
        assert_eq!(url_key("file:///etc/passwd"), None);
        assert_eq!(url_key(&format!("https://x.io/{}", "a".repeat(3000))), None);
        assert_ne!(url_key("https://x.io/a"), url_key("https://x.io/b"));
    }

    #[test]
    fn formats() {
        assert!(decode(vec![0x89, b'P', b'N', b'G', 0, 0]).is_some());
        assert!(decode(vec![0xff, 0xd8, 0xff]).is_some());
        assert!(decode(b"<html>".to_vec()).is_none());
    }
}
