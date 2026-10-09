//! Remote URL to repository identity. Handles `https://`, `ssh://`, `git://` and scp-style
//! `user@host:path` remotes; credentials in the URL are dropped, `.git` is stripped.
//!
//! - GitHub: `github.com/owner/repo` (also `ssh.github.com` on port 443).
//! - GitLab and unknown hosts: `owner` is every segment but the last (subgroups).
//! - Azure DevOps: `dev.azure.com/org/project/_git/repo`, `ssh.dev.azure.com:v3/org/project/repo`,
//!   legacy `org.visualstudio.com[/DefaultCollection]/project/_git/repo`.

use accounts::HostKind;
use percent_encoding::percent_decode_str;
use url::Url;

/// Which repository on which host a remote points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoId {
    /// Lowercase web host name (ssh aliases mapped to it).
    pub host: String,
    /// Non-default web port, only known from `http(s)` remotes.
    pub web_port: Option<u16>,
    /// User, organisation, group path (`group/subgroup`) or Azure organisation.
    pub owner: String,
    /// Azure DevOps project.
    pub project: Option<String>,
    pub repo: String,
}

impl RepoId {
    /// `None` for local paths and anything that is not a repository URL.
    pub fn parse(remote: &str) -> Option<Self> {
        let remote = remote.trim();
        let (scheme, host, port, path) = if remote.contains("://") {
            let url = Url::parse(remote).ok()?;
            if !matches!(
                url.scheme(),
                "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
            ) {
                return None;
            }
            let web = matches!(url.scheme(), "https" | "http");
            let port = if web { url.port() } else { None };
            let host = url.host_str()?.to_string();
            (url.scheme().to_string(), host, port, url.path().to_string())
        } else {
            let (host, path) = scp(remote)?;
            ("ssh".to_string(), host.to_string(), None, path.to_string())
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
            .collect();
        if let Some(last) = segments.last_mut() {
            if let Some(stripped) = last.strip_suffix(".git") {
                *last = stripped.to_string();
            }
            if last.is_empty() {
                segments.pop();
            }
        }
        let ssh = !matches!(scheme.as_str(), "https" | "http");
        Self::from_parts(&host, port, &segments, ssh)
    }

    fn from_parts(host: &str, web_port: Option<u16>, seg: &[String], ssh: bool) -> Option<Self> {
        let id = |host: &str, owner: &str, project: Option<&str>, repo: &str| {
            (!owner.is_empty() && !repo.is_empty()).then(|| RepoId {
                host: host.to_string(),
                web_port,
                owner: owner.to_string(),
                project: project.map(str::to_string),
                repo: repo.to_string(),
            })
        };
        // Azure DevOps over SSH: `v3/org/project/repo`.
        if ssh && matches!(host, "ssh.dev.azure.com" | "vs-ssh.visualstudio.com") {
            let [v3, org, project, repo] = seg else {
                return None;
            };
            if v3 != "v3" {
                return None;
            }
            let web = if host == "ssh.dev.azure.com" {
                "dev.azure.com".to_string()
            } else {
                format!("{}.visualstudio.com", org.to_ascii_lowercase())
            };
            return id(&web, org, Some(project), repo);
        }
        let azure_legacy = host.strip_suffix(".visualstudio.com");
        if host == "dev.azure.com" || azure_legacy.is_some() {
            let git = seg.iter().position(|s| s == "_git")?;
            let repo = seg.get(git + 1)?;
            let (org, rest) = match azure_legacy {
                Some(org) => (org.to_string(), &seg[..git]),
                None => (seg.first()?.clone(), seg.get(1..git).unwrap_or_default()),
            };
            // `DefaultCollection` is a legacy path prefix, not a project.
            let rest: Vec<&String> = rest.iter().filter(|s| *s != "DefaultCollection").collect();
            let project = rest.last().map_or(repo.as_str(), |p| p.as_str());
            return id(host, &org, Some(project), repo);
        }
        let host = if host == "ssh.github.com" {
            "github.com"
        } else {
            host
        };
        if seg.len() < 2 {
            return None;
        }
        if HostKind::of_host(host) == HostKind::GitHub {
            return id(host, &seg[0], None, &seg[1]);
        }
        let (repo, owner) = seg.split_last()?;
        id(host, &owner.join("/"), None, repo)
    }

    /// The hosted service, from the host name (self-hosted servers are `Generic`; use the
    /// matching account's kind for those).
    pub fn kind(&self) -> HostKind {
        HostKind::of_host(&self.host)
    }

    /// Web base URL the account for this repo would have, e.g. `https://github.com`,
    /// `https://gitlab.example.com:8443`, `https://dev.azure.com/org`.
    pub fn web_base(&self) -> String {
        let port = self.web_port.map(|p| format!(":{p}")).unwrap_or_default();
        if self.host == "dev.azure.com" {
            format!("https://dev.azure.com/{}", self.owner)
        } else {
            format!("https://{}{port}", self.host)
        }
    }

    /// `owner/repo`, or `org/project/repo` on Azure DevOps.
    pub fn full_name(&self) -> String {
        match &self.project {
            Some(project) => format!("{}/{project}/{}", self.owner, self.repo),
            None => format!("{}/{}", self.owner, self.repo),
        }
    }
}

/// `[user@]host:path`, not a Windows drive (`C:\x`) or a local path.
fn scp(remote: &str) -> Option<(&str, &str)> {
    let (before, path) = remote.split_once(':')?;
    if before.contains('/') || before.contains('\\') || path.starts_with("//") {
        return None;
    }
    let host = before.rsplit_once('@').map_or(before, |(_, h)| h);
    (host.len() > 1 && !path.is_empty()).then_some((host, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> RepoId {
        RepoId::parse(url).unwrap_or_else(|| panic!("{url} should parse"))
    }

    fn gh(owner: &str, repo: &str) -> RepoId {
        RepoId {
            host: "github.com".into(),
            web_port: None,
            owner: owner.into(),
            project: None,
            repo: repo.into(),
        }
    }

    #[test]
    fn github_forms() {
        let want = gh("InfernoDragon0", "GitDragon");
        for url in [
            "https://github.com/InfernoDragon0/GitDragon.git",
            "https://github.com/InfernoDragon0/GitDragon",
            "https://github.com/InfernoDragon0/GitDragon/",
            "https://user:ghp_secret@GitHub.com/InfernoDragon0/GitDragon.git",
            "http://github.com/InfernoDragon0/GitDragon",
            "git@github.com:InfernoDragon0/GitDragon.git",
            "github.com:InfernoDragon0/GitDragon",
            "ssh://git@github.com/InfernoDragon0/GitDragon.git",
            "ssh://git@ssh.github.com:443/InfernoDragon0/GitDragon.git",
            "git://github.com/InfernoDragon0/GitDragon.git",
            "  git@github.com:InfernoDragon0/GitDragon.git\n",
        ] {
            assert_eq!(parse(url), want, "{url}");
        }
        assert_eq!(want.web_base(), "https://github.com");
        assert_eq!(want.full_name(), "InfernoDragon0/GitDragon");
        assert_eq!(want.kind(), HostKind::GitHub);
    }

    #[test]
    fn gitlab_subgroups_and_ports() {
        let id = parse("https://gitlab.example.com:8443/group/sub/project.git");
        assert_eq!(id.host, "gitlab.example.com");
        assert_eq!(id.web_port, Some(8443));
        assert_eq!(id.owner, "group/sub");
        assert_eq!(id.repo, "project");
        assert_eq!(id.web_base(), "https://gitlab.example.com:8443");
        // The SSH port is not the web port.
        let id = parse("ssh://git@gitlab.example.com:2222/group/sub/project.git");
        assert_eq!(id.web_port, None);
        assert_eq!(id.owner, "group/sub");
        let id = parse("git@gitlab.com:a/b/c/d.git");
        assert_eq!((id.owner.as_str(), id.repo.as_str()), ("a/b/c", "d"));
        assert_eq!(id.kind(), HostKind::GitLab);
    }

    #[test]
    fn azure_devops_forms() {
        let want = RepoId {
            host: "dev.azure.com".into(),
            web_port: None,
            owner: "org".into(),
            project: Some("My Project".into()),
            repo: "repo".into(),
        };
        assert_eq!(
            parse("https://org@dev.azure.com/org/My%20Project/_git/repo"),
            want
        );
        assert_eq!(
            parse("git@ssh.dev.azure.com:v3/org/My%20Project/repo"),
            want
        );
        assert_eq!(
            parse("ssh://git@ssh.dev.azure.com/v3/org/My%20Project/repo"),
            want
        );
        assert_eq!(want.web_base(), "https://dev.azure.com/org");
        assert_eq!(want.full_name(), "org/My Project/repo");
        // Project-less: the repo shares the project's name.
        let id = parse("https://dev.azure.com/org/_git/repo");
        assert_eq!(id.project.as_deref(), Some("repo"));

        let legacy = parse("https://org.visualstudio.com/DefaultCollection/Proj/_git/repo");
        assert_eq!(legacy.host, "org.visualstudio.com");
        assert_eq!(legacy.owner, "org");
        assert_eq!(legacy.project.as_deref(), Some("Proj"));
        assert_eq!(legacy.web_base(), "https://org.visualstudio.com");
        assert_eq!(parse("https://org.visualstudio.com/Proj/_git/repo"), legacy);
        assert_eq!(
            parse("org@vs-ssh.visualstudio.com:v3/org/Proj/repo"),
            legacy
        );
        assert_eq!(legacy.kind(), HostKind::AzureDevOps);
    }

    #[test]
    fn rejects_non_remotes() {
        for url in [
            "",
            "C:\\repos\\thing",
            "C:/repos/thing",
            "/home/me/repo.git",
            "../other",
            "file:///srv/git/repo.git",
            "https://github.com/onlyowner",
            "https://dev.azure.com/org/project",
            "git@ssh.dev.azure.com:v2/org/p/r",
            "not a url",
        ] {
            assert_eq!(RepoId::parse(url), None, "{url}");
        }
    }
}
