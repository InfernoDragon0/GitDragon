//! Finding and spawning Claude Code's ACP adapter (`@zed-industries/claude-code-acp`, a Node
//! package) safely.
//!
//! - The program is always an absolute path: an explicit override, or a match in an *absolute*
//!   `PATH` entry ([`git_core::detect::absolute_path_dirs`]). Never a bare name.
//! - npm's launcher shims (`claude-code-acp.cmd` on Windows, a symlink to a `#!/usr/bin/env node`
//!   script elsewhere) are unwrapped: we run an absolutely-resolved `node` with the package's
//!   own entry script, so neither `cmd.exe` nor `env` looks anything up on `PATH`.
//! - `npx --yes @zed-industries/claude-code-acp` (downloads on first use) only when the user
//!   turned it on in Settings; `npx` itself is resolved absolutely too.
//! - The child gets a scrubbed environment ([`scrub_env`]): none of GitDragon's own variables,
//!   no askpass hooks, no forge tokens, no git config injection, and `PATH` reduced to its
//!   absolute entries.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Launcher names the adapter package has shipped under.
pub const BIN_NAMES: [&str; 2] = ["claude-code-acp", "claude-agent-acp"];
/// What `npx` runs when the user allows it.
pub const NPX_PACKAGE: &str = "@zed-industries/claude-code-acp";

/// How to start the adapter: an absolute program plus fixed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// For the UI: where it came from.
    pub source: AdapterSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterSource {
    /// The path set in Settings.
    Override,
    /// An installed `claude-code-acp` on `PATH`.
    Installed,
    /// `npx --yes …` (opt-in).
    Npx,
}

/// Where to look; built from the real environment by [`Lookup::from_env`].
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    /// Absolute path to the adapter (or its launcher) from Settings.
    pub override_path: Option<PathBuf>,
    /// Allow `npx --yes @zed-industries/claude-code-acp`.
    pub allow_npx: bool,
    /// The `PATH` value to search (only its absolute entries are used).
    pub path_var: Option<OsString>,
}

impl Lookup {
    pub fn from_env(override_path: Option<PathBuf>, allow_npx: bool) -> Self {
        Self {
            override_path,
            allow_npx,
            path_var: std::env::var_os("PATH"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FindError {
    #[error("the adapter path in Settings must be absolute: {0}")]
    OverrideNotAbsolute(PathBuf),
    #[error("the adapter path in Settings does not exist: {0}")]
    OverrideMissing(PathBuf),
    #[error("Claude Code's ACP adapter (claude-code-acp) was not found on PATH")]
    NotFound,
    #[error("found {0}, but not an absolute `node` to run it with")]
    NoNode(PathBuf),
}

/// Resolves the adapter command, or says why there is none.
pub fn find(lookup: &Lookup) -> Result<AdapterCommand, FindError> {
    let dirs = git_core::detect::absolute_path_dirs(lookup.path_var.clone());
    if let Some(path) = &lookup.override_path {
        if !path.is_absolute() {
            return Err(FindError::OverrideNotAbsolute(path.clone()));
        }
        if !path.is_file() {
            return Err(FindError::OverrideMissing(path.clone()));
        }
        return launch(path, &dirs, AdapterSource::Override);
    }
    for dir in &dirs {
        for name in BIN_NAMES {
            for file in launcher_names(name) {
                let candidate = dir.join(&file);
                if candidate.is_file() {
                    return launch(&candidate, &dirs, AdapterSource::Installed);
                }
            }
        }
    }
    if lookup.allow_npx {
        for file in launcher_names("npx") {
            if let Some(npx) = dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file()) {
                return Ok(AdapterCommand {
                    program: npx,
                    args: vec!["--yes".into(), NPX_PACKAGE.into()],
                    source: AdapterSource::Npx,
                });
            }
        }
    }
    Err(FindError::NotFound)
}

/// File names a launcher can have in a `PATH` directory.
fn launcher_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    }
}

/// The command for a found launcher: a native executable runs as is; an npm shim or a Node
/// script runs as `<absolute node> <entry script>`.
fn launch(
    found: &Path,
    dirs: &[PathBuf],
    source: AdapterSource,
) -> Result<AdapterCommand, FindError> {
    let direct = |program: &Path| AdapterCommand {
        program: program.to_path_buf(),
        args: Vec::new(),
        source,
    };
    // Not a script (a native binary), or a .cmd we cannot unwrap: run it by its absolute path
    // (std quotes a .cmd for cmd.exe; there are no arguments to inject into).
    let Some(script) = script_for(found) else {
        return Ok(direct(found));
    };
    let node = node_for(found, dirs).ok_or_else(|| FindError::NoNode(found.to_path_buf()))?;
    Ok(AdapterCommand {
        program: node,
        args: vec![script.into_os_string()],
        source,
    })
}

fn is_cmd_shim(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// The Node entry script behind a launcher, if it is one.
fn script_for(found: &Path) -> Option<PathBuf> {
    if is_cmd_shim(found) {
        // npm global layout: <prefix>/<bin>.cmd next to <prefix>/node_modules/<pkg>.
        let dir = found.parent()?;
        let bin = found.file_stem()?.to_str()?;
        return npm_package_entry(&dir.join("node_modules"), bin);
    }
    // Unix: the launcher is a symlink into the package; run the target with node when it is a
    // script (`#!/usr/bin/env node` would search PATH).
    let real = std::fs::canonicalize(found).ok()?;
    let is_js = real
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| matches!(e, "js" | "mjs" | "cjs"));
    if is_js || shebang_mentions_node(&real) {
        return Some(real);
    }
    None
}

fn shebang_mentions_node(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = [0u8; 128];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let n = file.read(&mut head).unwrap_or(0);
    let head = &head[..n];
    head.starts_with(b"#!") && {
        let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
        String::from_utf8_lossy(line).contains("node")
    }
}

/// `<modules>/@zed-industries/<pkg>/package.json`'s `bin` entry for `bin`, kept inside the
/// package directory.
pub fn npm_package_entry(modules: &Path, bin: &str) -> Option<PathBuf> {
    for pkg in BIN_NAMES {
        let dir = modules.join("@zed-industries").join(pkg);
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let rel = match &json["bin"] {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Object(map) => map
                .get(bin)
                .or_else(|| map.values().next())
                .and_then(|v| v.as_str()),
            _ => None,
        };
        let Some(rel) = rel else { continue };
        let entry = dir.join(rel);
        let (Ok(entry), Ok(root)) = (std::fs::canonicalize(&entry), std::fs::canonicalize(&dir))
        else {
            continue;
        };
        if entry.starts_with(&root) && entry.is_file() {
            return Some(entry);
        }
    }
    None
}

/// `node` next to the launcher (what npm's shim prefers), else the first on absolute `PATH`.
fn node_for(found: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    let names = if cfg!(windows) {
        vec!["node.exe"]
    } else {
        vec!["node"]
    };
    let beside = found.parent().into_iter().map(Path::to_path_buf);
    beside
        .chain(dirs.iter().cloned())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_absolute() && p.is_file())
}

/// Exact (case-insensitive) names never passed to the adapter.
const SCRUB_EXACT: &[&str] = &[
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "GIT_TERMINAL_PROMPT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITHUB_PAT",
    "GITLAB_TOKEN",
    "GL_TOKEN",
    "GITLAB_PRIVATE_TOKEN",
    "CI_JOB_TOKEN",
    "AZURE_DEVOPS_EXT_PAT",
    "AZURE_DEVOPS_TOKEN",
    "SYSTEM_ACCESSTOKEN",
    "BITBUCKET_TOKEN",
    "BITBUCKET_APP_PASSWORD",
    "GCM_CREDENTIAL_STORE",
];

/// Prefixes (case-insensitive) never passed: GitDragon's own variables (askpass socket and key
/// among them) and git's per-process config injection.
const SCRUB_PREFIX: &[&str] = &["GITDRAGON_", "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Should `name` be kept out of the adapter's environment?
pub fn is_scrubbed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SCRUB_EXACT.contains(&upper.as_str()) || SCRUB_PREFIX.iter().any(|p| upper.starts_with(p))
}

/// The adapter's environment: `vars` minus [`is_scrubbed`] names, with `PATH` cut down to its
/// absolute entries. The user's own `ANTHROPIC_API_KEY` (Claude Code's credential, not ours)
/// passes through; GitDragon's keychain key never does.
pub fn scrub_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(k, _)| !is_scrubbed(&k.to_string_lossy()))
        .filter_map(|(k, v)| {
            if k.to_string_lossy().eq_ignore_ascii_case("PATH") {
                let dirs = git_core::detect::absolute_path_dirs(Some(v));
                let joined = std::env::join_paths(dirs).ok()?;
                Some((k, joined))
            } else {
                Some((k, v))
            }
        })
        .collect()
}

/// Spawns the adapter in `cwd` with piped stdio and a scrubbed environment. Killed on drop.
pub fn spawn(cmd: &AdapterCommand, cwd: &Path) -> std::io::Result<async_process::Child> {
    if !cmd.program.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "adapter program must be an absolute path",
        ));
    }
    let mut std_cmd = std::process::Command::new(&cmd.program);
    std_cmd
        .args(&cmd.args)
        .current_dir(cwd)
        .env_clear()
        .envs(scrub_env(std::env::vars_os()));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std_cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // async_process drops stdio set before the conversion (lesson L2): set it after.
    let mut command = async_process::Command::from(std_cmd);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn scrubs_credentials_and_our_variables() {
        let abs = if cfg!(windows) { r"C:\bin" } else { "/usr/bin" };
        let path =
            std::env::join_paths([PathBuf::from(abs), PathBuf::from("rel"), PathBuf::from(".")])
                .unwrap();
        let mut vars = os(&[
            ("GITDRAGON_ASKPASS_SOCKET", "sock"),
            ("GitDragon_Askpass_Key", "k"),
            ("GIT_ASKPASS", "x"),
            ("GH_TOKEN", "t"),
            ("github_token", "t"),
            (
                "GIT_CONFIG_PARAMETERS",
                "'http.extraheader=Authorization: x'",
            ),
            ("GIT_CONFIG_KEY_0", "http.extraheader"),
            ("GIT_CONFIG_VALUE_0", "secret"),
            ("HOME", "/home/me"),
            ("ANTHROPIC_API_KEY", "user-own"),
        ]);
        vars.push(("PATH".into(), path));
        let out = scrub_env(vars);
        let names: Vec<String> = out
            .iter()
            .map(|(k, _)| k.to_string_lossy().into())
            .collect();
        assert_eq!(names, ["HOME", "ANTHROPIC_API_KEY", "PATH"]);
        let path = &out[2].1;
        let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
        assert_eq!(dirs, [PathBuf::from(abs)]);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gitdragon-ai-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn relative_path_entries_are_ignored() {
        let dir = temp_dir("rel");
        let rel = PathBuf::from("adapter-rel-dir");
        let lookup = Lookup {
            override_path: None,
            allow_npx: true,
            path_var: Some(std::env::join_paths([rel, PathBuf::from(".")]).unwrap()),
        };
        assert_eq!(find(&lookup), Err(FindError::NotFound));
        _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn override_must_be_absolute_and_exist() {
        let rel = Lookup {
            override_path: Some("claude-code-acp".into()),
            ..Lookup::default()
        };
        assert!(matches!(find(&rel), Err(FindError::OverrideNotAbsolute(_))));
        let missing = std::env::temp_dir().join("gitdragon-no-such-adapter-xyz");
        let gone = Lookup {
            override_path: Some(missing),
            ..Lookup::default()
        };
        assert!(matches!(find(&gone), Err(FindError::OverrideMissing(_))));
    }

    #[test]
    fn finds_installed_launcher_and_unwraps_npm_shim() {
        let dir = temp_dir("found");
        let pkg = dir.join("node_modules/@zed-industries/claude-code-acp");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@zed-industries/claude-code-acp","bin":{"claude-code-acp":"dist/index.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("dist/index.js"), "#!/usr/bin/env node\n").unwrap();
        let node = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        std::fs::write(&node, "").unwrap();
        let launcher = if cfg!(windows) {
            let shim = dir.join("claude-code-acp.cmd");
            std::fs::write(&shim, "@echo off\r\n").unwrap();
            shim
        } else {
            let link = dir.join("claude-code-acp");
            #[cfg(unix)]
            std::os::unix::fs::symlink(pkg.join("dist/index.js"), &link).unwrap();
            link
        };
        assert!(launcher.exists());
        let lookup = Lookup {
            override_path: None,
            allow_npx: false,
            path_var: Some(std::env::join_paths([&dir]).unwrap()),
        };
        let cmd = find(&lookup).unwrap();
        assert_eq!(cmd.source, AdapterSource::Installed);
        assert_eq!(cmd.program, node);
        assert!(cmd.program.is_absolute());
        let script = PathBuf::from(&cmd.args[0]);
        assert!(script.ends_with("index.js"), "{script:?}");
        _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn package_entry_cannot_escape_the_package() {
        let dir = temp_dir("escape");
        let pkg = dir.join("node_modules/@zed-industries/claude-code-acp");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(dir.join("evil.js"), "").unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"bin":{"claude-code-acp":"../../../evil.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            npm_package_entry(&dir.join("node_modules"), "claude-code-acp"),
            None
        );
        _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn npx_only_when_allowed() {
        let dir = temp_dir("npx");
        let npx = dir.join(if cfg!(windows) { "npx.cmd" } else { "npx" });
        std::fs::write(&npx, "").unwrap();
        let mut lookup = Lookup {
            override_path: None,
            allow_npx: false,
            path_var: Some(std::env::join_paths([&dir]).unwrap()),
        };
        assert_eq!(find(&lookup), Err(FindError::NotFound));
        lookup.allow_npx = true;
        let cmd = find(&lookup).unwrap();
        assert_eq!(cmd.source, AdapterSource::Npx);
        assert_eq!(cmd.program, npx);
        assert_eq!(
            cmd.args,
            [OsString::from("--yes"), OsString::from(NPX_PACKAGE)]
        );
        _ = std::fs::remove_dir_all(dir);
    }
}
