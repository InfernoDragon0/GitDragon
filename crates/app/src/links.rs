//! Opening web links: only plain http(s) links are opened, and links to a host the user has no
//! account on are shown in full for confirmation first.

use gpui_kit::{App, Window};
use url::Url;

use crate::{dialogs, toast};

/// Hosts that need no confirmation besides the ones of the signed-in accounts.
const KNOWN_HOSTS: &[&str] = &["github.com", "gitlab.com", "dev.azure.com"];

/// `raw` as a URL if it is an http(s) link with a host and no user name or password.
pub fn parse_web_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.username().is_empty()
        && url.password().is_none();
    plain.then_some(url)
}

/// Is `url`'s host one of `known` (the account hosts) or [`KNOWN_HOSTS`]?
fn is_known_host(url: &Url, known: &[String]) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    KNOWN_HOSTS.iter().any(|k| k.eq_ignore_ascii_case(host))
        || known.iter().any(|k| k.eq_ignore_ascii_case(host))
}

/// Hosts of the signed-in accounts.
fn account_hosts(cx: &App) -> Vec<String> {
    crate::accounts::accounts(cx)
        .map(|accounts| {
            accounts
                .list()
                .iter()
                .filter_map(|a| ::accounts::model::Origin::of(&a.base_url))
                .map(|o| o.host)
                .collect()
        })
        .unwrap_or_default()
}

/// Opens `raw` in the browser. Other schemes are refused with a toast. A host without an
/// account asks first when a `window` is at hand and is refused otherwise.
pub fn open(raw: &str, window: Option<&mut Window>, cx: &mut App) {
    open_with(raw, None, window, cx);
}

/// Like [`open`], also treating the host of `base_url` (an account being added) as known.
pub fn open_for_base(raw: &str, base_url: &str, window: Option<&mut Window>, cx: &mut App) {
    open_with(raw, Some(base_url), window, cx);
}

fn open_with(raw: &str, base_url: Option<&str>, window: Option<&mut Window>, cx: &mut App) {
    let Some(url) = parse_web_url(raw) else {
        toast::error(
            cx,
            "Link not opened",
            &format!("Only http and https links are opened: {raw}"),
        );
        return;
    };
    let mut known = account_hosts(cx);
    known.extend(
        base_url
            .and_then(::accounts::model::Origin::of)
            .map(|o| o.host),
    );
    let target = url.to_string();
    if is_known_host(&url, &known) {
        cx.open_url(&target);
        return;
    }
    match window {
        Some(window) => {
            let shown = ui::sanitize_line(&target).into_owned();
            let body = format!(
                "This link leads to {}:\n\n{shown}",
                url.host_str().unwrap_or("")
            );
            dialogs::confirm(
                window,
                cx,
                "Open link?",
                body,
                "Open",
                false,
                move |_, cx| cx.open_url(&target),
            );
        }
        None => toast::error(
            cx,
            "Link not opened",
            &format!("The host is not one of your accounts: {target}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> bool {
        parse_web_url(raw).is_some()
    }

    #[test]
    fn only_plain_web_links_pass() {
        assert!(ok("https://github.com/a"));
        assert!(ok("http://github.com/a"));
        assert!(ok(" https://example.com:8443/x?y=1#z "));
        assert!(!ok("file:///C:/Windows/System32/calc.exe"));
        assert!(!ok("javascript:alert(1)"));
        assert!(!ok("ms-settings:network"));
        assert!(!ok(r"\\host\share\x.exe"));
        assert!(!ok(r"C:\x.exe"));
        assert!(!ok("https://user:pw@example.com/"));
        assert!(!ok("https://user@example.com/"));
        assert!(!ok("https://:pw@example.com/"));
        assert!(!ok("https://"));
        assert!(!ok(""));
        assert!(!ok("git-lfs.com"));
    }

    #[test]
    fn known_hosts() {
        let known = vec!["git.example.com".to_string()];
        let url = |s: &str| parse_web_url(s).unwrap();
        assert!(is_known_host(&url("https://GitHub.com/a"), &known));
        assert!(is_known_host(&url("https://dev.azure.com/o"), &known));
        assert!(is_known_host(
            &url("https://git.example.com:8443/a"),
            &known
        ));
        assert!(!is_known_host(
            &url("https://github.com.evil.test/a"),
            &known
        ));
        assert!(!is_known_host(&url("https://example.com/a"), &known));
    }
}
