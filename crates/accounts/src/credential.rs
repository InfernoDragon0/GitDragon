//! Git's credential protocol (`git credential fill|approve|reject`), used by GCM mode to get a
//! token for API calls. Push / fetch need none of this: git asks the helper itself.

use git_core::{GitCommand, GitRunner};

use crate::model::Origin;
use crate::secret::Secret;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential value for {0} contains a newline or NUL")]
    Injection(&'static str),
    #[error("no credential available for {0}")]
    None(String),
    #[error(transparent)]
    Git(#[from] git_core::GitError),
}

/// What git returned for an origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: Secret,
}

fn checked(what: &'static str, value: &str) -> Result<(), CredentialError> {
    if value.contains(['\n', '\0']) {
        Err(CredentialError::Injection(what))
    } else {
        Ok(())
    }
}

/// The protocol request for `origin` (plus a credential for approve / reject).
pub fn encode(origin: &Origin, credential: Option<&Credential>) -> Result<String, CredentialError> {
    encode_with_path(origin, None, credential)
}

/// [`encode`] with a `path=` line (GCM scopes Azure DevOps sign-ins by organisation).
pub fn encode_with_path(
    origin: &Origin,
    path: Option<&str>,
    credential: Option<&Credential>,
) -> Result<String, CredentialError> {
    checked("protocol", &origin.scheme)?;
    checked("host", &origin.host)?;
    let default_port = url::Url::parse(&format!("{}://x", origin.scheme))
        .ok()
        .and_then(|u| u.port_or_known_default());
    let host = if default_port == Some(origin.port) {
        origin.host.clone()
    } else {
        format!("{}:{}", origin.host, origin.port)
    };
    let mut out = format!("protocol={}\nhost={host}\n", origin.scheme);
    if let Some(path) = path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
        checked("path", path)?;
        out.push_str(&format!("path={path}\n"));
    }
    if let Some(c) = credential {
        checked("username", &c.username)?;
        checked("password", c.password.expose())?;
        out.push_str(&format!(
            "username={}\npassword={}\n",
            c.username,
            c.password.expose()
        ));
    }
    out.push('\n');
    Ok(out)
}

/// `username` / `password` from git's `key=value` answer.
pub fn decode(output: &[u8]) -> Option<Credential> {
    let text = String::from_utf8_lossy(output);
    let mut username = None;
    let mut password = None;
    for line in text.lines() {
        match line.split_once('=') {
            Some(("username", v)) => username = Some(v.to_string()),
            Some(("password", v)) => password = Some(Secret::new(v)),
            _ => {}
        }
    }
    let password = password.filter(|p| !p.is_empty())?;
    Some(Credential {
        username: username.unwrap_or_default(),
        password,
    })
}

/// Asks git's credential helpers (GCM) for `origin`. GCM may show its own sign-in window.
pub async fn fill(
    runner: &GitRunner,
    cwd: &std::path::Path,
    origin: &Origin,
    path: Option<&str>,
) -> Result<Credential, CredentialError> {
    let input = encode_with_path(origin, path, None)?;
    let cmd = GitCommand::write(cwd)
        .args(["credential", "fill"])
        .stdin(input.into_bytes());
    let out = runner.run(cmd).await?;
    let credential = decode(&out).ok_or_else(|| CredentialError::None(origin.host.clone()))?;
    git_core::redact::register(credential.password.expose());
    Ok(credential)
}

/// Tells the helpers the credential worked (`approve`) or failed (`reject`, which makes GCM
/// forget it).
pub async fn report(
    runner: &GitRunner,
    cwd: &std::path::Path,
    origin: &Origin,
    path: Option<&str>,
    credential: &Credential,
    worked: bool,
) -> Result<(), CredentialError> {
    let input = encode_with_path(origin, path, Some(credential))?;
    let verb = if worked { "approve" } else { "reject" };
    let cmd = GitCommand::write(cwd)
        .args(["credential", verb])
        .stdin(input.into_bytes());
    runner.run(cmd).await?;
    Ok(())
}

/// Git's configured credential helpers (system + global, last wins in git), e.g. `manager`.
/// Run outside any repository so only user-wide settings count.
pub async fn configured_helpers(
    runner: &GitRunner,
    cwd: &std::path::Path,
) -> Result<Vec<String>, CredentialError> {
    let cmd = GitCommand::read(cwd).args(["config", "--get-all", "credential.helper"]);
    let out = runner.output(cmd).await?;
    // Exit 1 = not set.
    if !out.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // An empty entry resets the list (git's own rule).
    let mut helpers = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            helpers.clear();
        } else {
            helpers.push(line.to_string());
        }
    }
    Ok(helpers)
}

/// A sign-in Git Credential Manager has saved: where and as whom (never the secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSignIn {
    /// `github.com` or `dev.azure.com/infernodragon0`: host plus, for path-scoped entries,
    /// the first path segment (Azure organisation).
    pub place: String,
    pub username: String,
}

/// GCM's saved sign-ins from the Windows credential store (`cmdkey /list`, which prints targets
/// and usernames only). Empty on other platforms or when nothing is saved.
pub fn saved_sign_ins() -> Vec<SavedSignIn> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("cmdkey")
            .arg("/list")
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(out) if out.status.success() => parse_cmdkey(&String::from_utf8_lossy(&out.stdout)),
            _ => Vec::new(),
        }
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// `Target: LegacyGeneric:target=git:https://github.com` followed by `User: name`.
fn parse_cmdkey(text: &str) -> Vec<SavedSignIn> {
    let mut found: Vec<SavedSignIn> = Vec::new();
    let mut place: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if let Some(target) = line.strip_prefix("Target:") {
            place = target
                .split_once("git:")
                .and_then(|(_, url)| place_of(url.trim()));
        } else if let Some(user) = line.strip_prefix("User:")
            && let Some(p) = place.take()
        {
            let entry = SavedSignIn {
                place: p,
                username: user.trim().to_string(),
            };
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
    }
    found
}

/// `https://user@host/org/project/...` -> `host/org` (host only when there is no path).
fn place_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(match path.split('/').find(|s| !s.is_empty()) {
        Some(first) => format!("{host}/{first}"),
        None => host,
    })
}

/// A friendly name for a helper, and whether it stores secrets in plain text.
pub fn helper_label(helper: &str) -> (String, bool) {
    // A full path may contain spaces (`C:/Program Files/…/git-credential-manager.exe`): look
    // for the `git-credential-<name>` part first, else take the first word's file name.
    let name = match helper.find("git-credential-") {
        Some(at) => helper[at + "git-credential-".len()..]
            .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .next()
            .unwrap_or_default(),
        None => {
            let first = helper
                .trim_start_matches('!')
                .split_whitespace()
                .next()
                .unwrap_or(helper);
            first.rsplit(['/', '\\']).next().unwrap_or(first)
        }
    }
    .trim_end_matches(".exe");
    match name {
        "manager" | "manager-core" | "credential-manager" => {
            ("Git Credential Manager".into(), false)
        }
        "wincred" => ("Windows Credential Store".into(), false),
        "osxkeychain" => ("macOS Keychain".into(), false),
        "libsecret" | "gnome-keyring" => ("Secret Service keyring".into(), false),
        "cache" => ("In-memory cache".into(), false),
        "store" => ("Plain-text file (~/.git-credentials)".into(), true),
        other => (other.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdkey_listing() {
        let text = "
Currently stored credentials:

    Target: LegacyGeneric:target=git:https://github.com
    Type: Generic
    User: InfernoDragon0

    Target: LegacyGeneric:target=git:https://Personal Access Token@github.com
    Type: Generic
    User: Personal Access Token

    Target: LegacyGeneric:target=git:https://dev.azure.com/infernodragon0/Proj/_git/Proj
    Type: Generic
    User: infernodragon0

    Target: Domain:target=fileserver
    Type: Domain Password
    User: me
";
        let saved = parse_cmdkey(text);
        assert_eq!(
            saved,
            [
                SavedSignIn {
                    place: "github.com".into(),
                    username: "InfernoDragon0".into()
                },
                SavedSignIn {
                    place: "github.com".into(),
                    username: "Personal Access Token".into()
                },
                SavedSignIn {
                    place: "dev.azure.com/infernodragon0".into(),
                    username: "infernodragon0".into()
                },
            ]
        );
        assert!(!format!("{saved:?}").to_lowercase().contains("password:"));
    }

    #[test]
    fn helper_labels() {
        assert_eq!(helper_label("manager").0, "Git Credential Manager");
        assert_eq!(helper_label("manager-core").0, "Git Credential Manager");
        assert_eq!(
            helper_label("C:/Program Files/Git/mingw64/bin/git-credential-manager.exe").0,
            "Git Credential Manager"
        );
        assert_eq!(
            helper_label("store --file ~/.creds"),
            ("Plain-text file (~/.git-credentials)".into(), true)
        );
        assert_eq!(helper_label("osxkeychain").0, "macOS Keychain");
        assert_eq!(helper_label("!f() { echo x; }; f").0, "f()");
    }

    fn origin(url: &str) -> Origin {
        Origin::of(url).unwrap()
    }

    #[test]
    fn encodes_requests() {
        assert_eq!(
            encode(&origin("https://github.com/x"), None).unwrap(),
            "protocol=https\nhost=github.com\n\n"
        );
        assert_eq!(
            encode(&origin("https://git.example.com:8443"), None).unwrap(),
            "protocol=https\nhost=git.example.com:8443\n\n"
        );
        let c = Credential {
            username: "me".into(),
            password: Secret::new("tok"),
        };
        assert_eq!(
            encode(&origin("https://github.com"), Some(&c)).unwrap(),
            "protocol=https\nhost=github.com\nusername=me\npassword=tok\n\n"
        );
    }

    #[test]
    fn path_scoped_requests() {
        assert_eq!(
            encode_with_path(
                &origin("https://dev.azure.com"),
                Some("/infernodragon0/"),
                None
            )
            .unwrap(),
            "protocol=https\nhost=dev.azure.com\npath=infernodragon0\n\n"
        );
        assert!(
            encode_with_path(&origin("https://dev.azure.com"), Some("a\nhost=x"), None).is_err()
        );
    }

    #[test]
    fn rejects_injection() {
        let bad = Credential {
            username: "me\nhost=evil.io".into(),
            password: Secret::new("tok"),
        };
        assert!(matches!(
            encode(&origin("https://github.com"), Some(&bad)),
            Err(CredentialError::Injection("username"))
        ));
        let nul = Credential {
            username: "me".into(),
            password: Secret::new("t\0k"),
        };
        assert!(encode(&origin("https://github.com"), Some(&nul)).is_err());
    }

    #[test]
    fn decodes_answers() {
        let c =
            decode(b"protocol=https\nhost=github.com\nusername=me\npassword=gho_abc\n").unwrap();
        assert_eq!(c.username, "me");
        assert_eq!(c.password.expose(), "gho_abc");
        assert!(decode(b"protocol=https\nhost=github.com\n").is_none());
        assert!(decode(b"password=\n").is_none());
    }

    /// A store helper in a temp HOME answers `fill`, and `reject` removes the entry.
    #[test]
    fn fill_and_reject_through_git() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("creds");
        std::fs::write(&store, "https://me:s3cr3t-token-value@git.example.com\n").unwrap();
        let repo = dir.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        let runner = GitRunner::detect().unwrap();
        let git = |args: &[&str]| {
            async_io::block_on(runner.run(GitCommand::write(&repo).args(args))).unwrap()
        };
        git(&["init", "-q"]);
        let helper = format!(
            "store --file={}",
            store.to_string_lossy().replace('\\', "/")
        );
        // Only our helper: no system GCM prompts in tests.
        git(&["config", "credential.helper", ""]);
        git(&["config", "--add", "credential.helper", &helper]);

        let o = origin("https://git.example.com");
        let c = async_io::block_on(fill(&runner, &repo, &o, None)).unwrap();
        assert_eq!(c.username, "me");
        assert_eq!(c.password.expose(), "s3cr3t-token-value");
        // Filled secrets are redacted from then on.
        assert!(!git_core::redact::redact("x s3cr3t-token-value").contains("s3cr3t"));

        async_io::block_on(report(&runner, &repo, &o, None, &c, false)).unwrap();
        assert!(
            !std::fs::read_to_string(&store)
                .unwrap()
                .contains("git.example.com")
        );
    }
}
