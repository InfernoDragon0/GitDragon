//! "Install MCP server": adds a `gitdragon` entry for `gitdragon-mcp` to Claude Code's user
//! config (`~/.claude.json`, the same place `claude mcp add --scope user` writes) or Claude
//! Desktop's `claude_desktop_config.json`.
//!
//! The JSON is merged, never replaced: other servers and settings stay as they were (key order
//! too), a file that does not parse is left alone, the previous file is copied to
//! `<name>.gitdragon-bak` first, and the new one is written to a temp file and renamed in.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The key under `mcpServers`.
pub const SERVER_NAME: &str = "gitdragon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
}

impl Client {
    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeDesktop => "Claude Desktop",
        }
    }

    /// Where this client keeps its MCP servers.
    pub fn config_path(self) -> Option<PathBuf> {
        let home = etcetera::home_dir().ok()?;
        Some(match self {
            Client::ClaudeCode => home.join(".claude.json"),
            Client::ClaudeDesktop => desktop_dir(&home)?.join("claude_desktop_config.json"),
        })
    }
}

fn desktop_dir(home: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Claude"))
    } else if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/Claude"))
    } else {
        Some(home.join(".config/Claude"))
    }
}

/// `gitdragon-mcp` next to the running app, when it is there.
pub fn server_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe
        .parent()?
        .join(format!("gitdragon-mcp{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// The server entry: absolute command, the repository, and the writes flag only when asked.
pub fn entry(client: Client, exe: &Path, repo: &Path, allow_writes: bool) -> Value {
    let mut args = vec![
        Value::from("--repo"),
        Value::from(repo.display().to_string()),
    ];
    if allow_writes {
        args.push("--allow-writes".into());
    }
    let mut entry = Map::new();
    if client == Client::ClaudeCode {
        entry.insert("type".into(), "stdio".into());
    }
    entry.insert("command".into(), exe.display().to_string().into());
    entry.insert("args".into(), Value::Array(args));
    entry.insert("env".into(), json!({}));
    Value::Object(entry)
}

/// `existing` (a config file's text, or `None` when there is none) with
/// `mcpServers.<SERVER_NAME>` set to `entry`. Everything else is kept.
pub fn merge(existing: Option<&str>, entry: &Value) -> Result<String, String> {
    let mut root = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        None => Value::Object(Map::new()),
        Some(text) => serde_json::from_str::<Value>(text)
            .map_err(|e| format!("the config is not valid JSON ({e}); left unchanged"))?,
    };
    let root_obj = root
        .as_object_mut()
        .ok_or("the config is not a JSON object; left unchanged")?;
    let servers = root_obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .ok_or("`mcpServers` is not a JSON object; left unchanged")?;
    servers.insert(SERVER_NAME.into(), entry.clone());
    let mut text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

/// Backup file for `path`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".gitdragon-bak");
    path.with_file_name(name)
}

/// Merges `entry` into the config at `path`. Returns the backup's path when there was a file.
pub fn install(path: &Path, entry: &Value) -> Result<Option<PathBuf>, String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let merged = merge(existing.as_deref(), entry)?;
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let backup = if existing.is_some() {
        let backup = backup_path(path);
        std::fs::copy(path, &backup).map_err(io)?;
        Some(backup)
    } else {
        None
    };
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".gitdragon-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, merged).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        _ = std::fs::remove_file(&tmp);
        io(e)
    })?;
    Ok(backup)
}

/// What the confirm dialog shows: the file, the backup, and the exact entry.
pub fn preview(client: Client, path: &Path, entry: &Value) -> String {
    let json = serde_json::to_string_pretty(&json!({ SERVER_NAME: entry })).unwrap_or_default();
    format!(
        "Adds or replaces mcpServers.{SERVER_NAME} in {}\n(other servers are kept; the current \
         file is backed up to {} first). Restart {} afterwards.\n\n{json}",
        path.display(),
        backup_path(path).display(),
        client.label(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(writes: bool) -> Value {
        entry(
            Client::ClaudeCode,
            Path::new("/opt/gd/gitdragon-mcp"),
            Path::new("/src/repo"),
            writes,
        )
    }

    #[test]
    fn entry_adds_writes_only_when_asked() {
        let ro = sample_entry(false);
        assert_eq!(ro["args"], json!(["--repo", "/src/repo"]));
        assert_eq!(ro["type"], "stdio");
        assert_eq!(sample_entry(true)["args"][2], "--allow-writes");
        let desktop = entry(
            Client::ClaudeDesktop,
            Path::new("/x"),
            Path::new("/r"),
            false,
        );
        assert!(desktop.get("type").is_none());
    }

    #[test]
    fn merge_keeps_other_servers_and_settings() {
        let existing = r#"{"numStartups": 3, "mcpServers": {"other": {"command": "x"}}, "z": [1]}"#;
        let merged = merge(Some(existing), &sample_entry(false)).unwrap();
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["z"], json!([1]));
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            v["mcpServers"][SERVER_NAME]["command"],
            "/opt/gd/gitdragon-mcp"
        );
        // Key order is kept (serde_json preserve_order).
        assert!(merged.find("numStartups").unwrap() < merged.find("\"z\"").unwrap());
        // Re-running replaces our entry only.
        let again = merge(Some(&merged), &sample_entry(true)).unwrap();
        let v: Value = serde_json::from_str(&again).unwrap();
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 2);
        assert_eq!(v["mcpServers"][SERVER_NAME]["args"][2], "--allow-writes");
    }

    #[test]
    fn merge_refuses_what_it_cannot_understand() {
        assert!(merge(Some("{ not json"), &sample_entry(false)).is_err());
        assert!(merge(Some("[1, 2]"), &sample_entry(false)).is_err());
        assert!(merge(Some(r#"{"mcpServers": []}"#), &sample_entry(false)).is_err());
        let fresh = merge(None, &sample_entry(false)).unwrap();
        assert!(fresh.contains("mcpServers"));
    }

    #[test]
    fn install_backs_up_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Claude").join("claude_desktop_config.json");
        // No file yet: created, no backup.
        assert_eq!(install(&path, &sample_entry(false)).unwrap(), None);
        let original = r#"{"mcpServers": {"keep": {"command": "k"}}, "theme": "dark"}"#;
        std::fs::write(&path, original).unwrap();
        let backup = install(&path, &sample_entry(false)).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["keep"]["command"], "k");
        assert!(v["mcpServers"][SERVER_NAME].is_object());
        // A broken file is left exactly as it was.
        std::fs::write(&path, "{ broken").unwrap();
        assert!(install(&path, &sample_entry(false)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
        assert!(
            !dir.path()
                .join("Claude/claude_desktop_config.json.gitdragon-tmp")
                .exists()
        );
    }
}
