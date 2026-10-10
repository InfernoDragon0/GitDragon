//! `gitdragon-mcp`: GitDragon's git-core behind the Model Context Protocol, over stdio only.

pub mod server;
pub mod tools;

use std::path::PathBuf;

pub use server::GitServer;
pub use tools::GitTools;

/// What the command line asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Absolute paths of the repositories tools may touch.
    pub repos: Vec<PathBuf>,
    /// Enables the write tools.
    pub allow_writes: bool,
}

/// What `main` should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve(Config),
    Help,
    Version,
}

pub const USAGE: &str = "\
gitdragon-mcp: GitDragon's git tools as an MCP server (stdio)

USAGE:
    gitdragon-mcp --repo <ABSOLUTE PATH> [--repo <PATH>...] [--allow-writes]

OPTIONS:
    --repo <PATH>     A repository the tools may read (repeatable; absolute paths only)
    --allow-writes    Also offer stage, unstage, commit and create_branch (off by default)
    -h, --help        Print this help
    -V, --version     Print the version
";

/// Parses the arguments after the program name.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut config = Config::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let repo = match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--allow-writes" => {
                config.allow_writes = true;
                continue;
            }
            "--repo" => args.next().ok_or("--repo needs a path")?,
            other => match other.strip_prefix("--repo=") {
                Some(path) => path.to_string(),
                None => return Err(format!("unknown argument: {other}")),
            },
        };
        let path = PathBuf::from(&repo);
        if repo.is_empty() || !path.is_absolute() {
            return Err(format!("--repo must be an absolute path: {repo:?}"));
        }
        if !config.repos.contains(&path) {
            config.repos.push(path);
        }
    }
    if config.repos.is_empty() {
        return Err("at least one --repo <absolute path> is required".into());
    }
    Ok(Command::Serve(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    fn abs() -> String {
        std::env::temp_dir().display().to_string()
    }

    #[test]
    fn args_need_an_absolute_repo() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--repo"]).is_err());
        assert!(parse(&["--repo", "relative/dir"]).is_err());
        assert!(parse(&["--repo", ""]).is_err());
        assert!(parse(&["--repo", &abs(), "--git-dir=x"]).is_err());
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn writes_are_off_unless_asked() {
        let Ok(Command::Serve(config)) = parse(&["--repo", &abs()]) else {
            panic!("expected serve");
        };
        assert!(!config.allow_writes);
        let eq = format!("--repo={}", abs());
        let Ok(Command::Serve(config)) = parse(&[&eq, "--allow-writes", "--repo", &abs()]) else {
            panic!("expected serve");
        };
        assert!(config.allow_writes);
        assert_eq!(config.repos.len(), 1, "duplicates collapse");
    }
}
