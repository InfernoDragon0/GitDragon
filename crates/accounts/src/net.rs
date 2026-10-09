//! The one tokio runtime for network calls (design "Tech stack": GPUI executor for the app, a
//! single tokio worker for reqwest). Started on first use; awaiting [`spawn`]'s handle works
//! from any executor.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("gitdragon-net")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Runs `future` on the network runtime.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future)
}

/// Runs `future` to completion on the network runtime, blocking this thread. Only for threads
/// outside the runtime (the askpass connection threads).
pub fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

/// GETs a public resource (avatars) with a separate client that never carries credentials:
/// HTTPS only, up to 3 redirects, at most `limit` bytes. `Ok(None)` for 404 / 410.
pub async fn fetch_public(url: String, limit: usize) -> Result<Option<Vec<u8>>, String> {
    static PUBLIC: OnceLock<reqwest::Client> = OnceLock::new();
    let client = PUBLIC.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("GitDragon/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("HTTP client")
    });
    let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if response
        .content_length()
        .is_some_and(|n| n as usize > limit)
    {
        return Err("too large".into());
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("too large".into());
    }
    Ok(Some(bytes.to_vec()))
}

/// The shared HTTP client (rustls, GitDragon user agent, sane timeouts).
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("GitDragon/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            // Never follow a redirect to another host with our Authorization header.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client")
    })
}

#[cfg(test)]
mod tests {
    /// Live network: `cargo test -p accounts --lib net::tests -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn fetches_public_avatars() {
        let png = super::block_on(super::fetch_public(
            "https://avatars.githubusercontent.com/u/1?s=64&v=4".into(),
            512 * 1024,
        ))
        .unwrap()
        .expect("GitHub user 1 has an avatar");
        assert!(png.len() > 100);
        // Gravatar's d=404 for an unknown hash.
        let none = super::block_on(super::fetch_public(
            format!("https://gravatar.com/avatar/{}?s=64&d=404", "0".repeat(64)),
            512 * 1024,
        ))
        .unwrap();
        assert!(none.is_none());
        // Plain HTTP is refused.
        assert!(super::block_on(super::fetch_public("http://example.com".into(), 10)).is_err());
    }
}
