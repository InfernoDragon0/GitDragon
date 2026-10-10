//! `GIT_ASKPASS` bridge: git runs our own executable with a prompt such as
//! `Password for 'https://me@github.com': `; that helper process asks the running app over a
//! local socket and prints the answer.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::sync::Arc;

use interprocess::local_socket::{ListenerOptions, Stream, prelude::*};

use crate::model::Origin;
use crate::secret::Secret;

/// Env var naming the socket; its presence also tells `main` to run as the askpass helper.
pub const SOCKET_ENV: &str = "GITDRAGON_ASKPASS_SOCKET";
pub const KEY_ENV: &str = "GITDRAGON_ASKPASS_KEY";

/// Longest request line accepted (a prompt is a URL plus a few words).
const MAX_LINE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    Username,
    Password,
}

/// A parsed git prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub kind: AskKind,
    pub origin: Origin,
    /// `me` in `Password for 'https://me@github.com': `.
    pub username: Option<String>,
}

/// Parses git's `Username for '<url>': ` / `Password for '<url>': ` prompts. Anything else
/// (SSH passphrases, host key questions) is `None`.
pub fn parse_prompt(prompt: &str) -> Option<Ask> {
    let prompt = prompt.trim();
    let (kind, rest) = if let Some(rest) = prompt.strip_prefix("Username for '") {
        (AskKind::Username, rest)
    } else {
        (AskKind::Password, prompt.strip_prefix("Password for '")?)
    };
    let url = rest.strip_suffix("':")?;
    let parsed = url::Url::parse(url).ok()?;
    let username = Some(parsed.username())
        .filter(|u| !u.is_empty())
        .map(str::to_owned);
    Some(Ask {
        kind,
        origin: Origin::of(url)?,
        username,
    })
}

type Resolver = dyn Fn(&Ask) -> Option<Secret> + Send + Sync;

/// The app side: listens for helper requests for as long as the process lives.
pub struct AskpassServer {
    socket: String,
    key: Secret,
}

impl AskpassServer {
    /// Starts listening on a fresh socket. `resolve` answers a parsed prompt (username or
    /// token) or `None` to refuse.
    pub fn start(
        resolve: impl Fn(&Ask) -> Option<Secret> + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let socket = socket_name()?;
        let key = Secret::new(random_hex(32));
        let listener = ListenerOptions::new()
            .name(to_name(&socket)?)
            .create_sync()?;
        let resolve: Arc<Resolver> = Arc::new(resolve);
        let expected = key.clone();
        std::thread::Builder::new()
            .name("gitdragon-askpass".into())
            .spawn(move || {
                for stream in listener.incoming().filter_map(Result::ok) {
                    let (resolve, expected) = (resolve.clone(), expected.clone());
                    // One short-lived thread per request, so a stuck client blocks nothing.
                    _ = std::thread::Builder::new()
                        .name("gitdragon-askpass-conn".into())
                        .spawn(move || serve(stream, &expected, resolve.as_ref()));
                }
            })?;
        Ok(Self { socket, key })
    }

    /// Variables for git children: our executable as `GIT_ASKPASS`, plus socket and key. The
    /// key is not a token: it only lets the helper ask this process.
    pub fn env(&self, exe: &std::path::Path) -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_ASKPASS".into(), exe.as_os_str().to_owned()),
            (SOCKET_ENV.into(), self.socket.clone().into()),
            (KEY_ENV.into(), self.key.expose().into()),
        ]
    }

    #[cfg(test)]
    fn connection(&self) -> (String, Secret) {
        (self.socket.clone(), self.key.clone())
    }
}

/// One request: `<key>\n<prompt>\n` -> `OK <answer>\n` or `NO\n`.
fn serve(stream: Stream, expected: &Secret, resolve: &Resolver) {
    let mut reader = BufReader::new(stream);
    let line = |reader: &mut BufReader<Stream>| -> Option<String> {
        let mut buf = String::new();
        reader.by_ref().take(MAX_LINE).read_line(&mut buf).ok()?;
        buf.strip_suffix('\n').map(str::to_owned)
    };
    let answer = (|| {
        let key = line(&mut reader)?;
        if !constant_time_eq(key.as_bytes(), expected.expose().as_bytes()) {
            tracing::warn!("askpass request with a wrong key refused");
            return None;
        }
        let prompt = line(&mut reader)?;
        let ask = parse_prompt(&prompt)?;
        let value = resolve(&ask)?;
        (!value.expose().contains(['\n', '\r', '\0'])).then_some(value)
    })();
    let mut stream = reader.into_inner();
    let reply = match &answer {
        Some(value) => format!("OK {}\n", value.expose()),
        None => "NO\n".to_string(),
    };
    _ = stream.write_all(reply.as_bytes());
    _ = stream.flush();
}

/// The helper side (`main` calls this when [`SOCKET_ENV`] is set): asks the app and returns
/// the answer for git's stdout, or `None` (exit non-zero: git then fails the auth).
pub fn ask_app(prompt: &str) -> Option<Secret> {
    let socket = std::env::var(SOCKET_ENV).ok()?;
    let key = Secret::new(std::env::var(KEY_ENV).ok()?);
    request(&socket, &key, prompt)
}

fn request(socket: &str, key: &Secret, prompt: &str) -> Option<Secret> {
    if prompt.contains(['\n', '\0']) {
        return None;
    }
    let mut stream = Stream::connect(to_name(socket).ok()?).ok()?;
    stream
        .write_all(format!("{}\n{prompt}\n", key.expose()).as_bytes())
        .ok()?;
    stream.flush().ok()?;
    let mut reply = String::new();
    BufReader::new(stream)
        .take(MAX_LINE)
        .read_line(&mut reply)
        .ok()?;
    let value = reply.strip_suffix('\n')?.strip_prefix("OK ")?;
    Some(Secret::new(value))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random source");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Windows: a pipe name (`\\.\pipe\gitdragon-askpass-<random>`). Unix: a socket file in a
/// fresh 0700 directory.
fn socket_name() -> std::io::Result<String> {
    let id = random_hex(12);
    #[cfg(windows)]
    {
        Ok(format!("gitdragon-askpass-{id}"))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("gitdragon-{id}"));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(dir.join("askpass.sock").to_string_lossy().into_owned())
    }
}

fn to_name(socket: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(windows)]
    {
        socket.to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        socket.to_fs_name::<interprocess::local_socket::GenericFilePath>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts() {
        let a = parse_prompt("Username for 'https://github.com': ").unwrap();
        assert_eq!(a.kind, AskKind::Username);
        assert_eq!(a.origin, Origin::of("https://github.com").unwrap());
        assert_eq!(a.username, None);
        let p = parse_prompt("Password for 'https://me@gitlab.example.com:8443': ").unwrap();
        assert_eq!(p.kind, AskKind::Password);
        assert_eq!(p.origin.port, 8443);
        assert_eq!(p.username.as_deref(), Some("me"));
        assert!(parse_prompt("Enter passphrase for key '/home/me/.ssh/id_ed25519': ").is_none());
        assert!(parse_prompt("Are you sure you want to continue connecting (yes/no)?").is_none());
        assert!(parse_prompt("Password for 'not a url': ").is_none());
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    /// Server and helper end to end: right key and exact host get the token; wrong key, other
    /// hosts and look-alike hosts get nothing.
    #[test]
    fn server_answers_only_exact_hosts_with_the_key() {
        let github = Origin::of("https://github.com").unwrap();
        let server = AskpassServer::start(move |ask| {
            (ask.origin == github).then(|| match ask.kind {
                AskKind::Username => Secret::new("mushr"),
                AskKind::Password => Secret::new("ghp_CANARYtoken123"),
            })
        })
        .unwrap();
        let (socket, key) = server.connection();
        let ask = |key: &Secret, prompt: &str| request(&socket, key, prompt);

        assert_eq!(
            ask(&key, "Username for 'https://github.com': ").map(|s| s.expose().to_string()),
            Some("mushr".into())
        );
        assert_eq!(
            ask(&key, "Password for 'https://mushr@github.com': ").map(|s| s.expose().to_string()),
            Some("ghp_CANARYtoken123".into())
        );
        assert!(ask(&key, "Password for 'https://github.com.evil.io': ").is_none());
        assert!(ask(&key, "Password for 'http://github.com': ").is_none());
        assert!(ask(&key, "Password for 'https://submodule.example.com': ").is_none());
        assert!(ask(&Secret::new("wrong"), "Password for 'https://github.com': ").is_none());
        assert!(ask(&key, "Password for 'https://github.com':\nextra").is_none());

        let env = server.env(std::path::Path::new("gitdragon.exe"));
        assert!(env.iter().any(|(k, _)| k == "GIT_ASKPASS"));
        assert!(
            env.iter()
                .all(|(_, v)| !v.to_string_lossy().contains("CANARY"))
        );
    }
}
