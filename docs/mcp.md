# gitdragon-mcp

`gitdragon-mcp` is a small [Model Context Protocol](https://modelcontextprotocol.io) server that gives MCP clients
(Claude Code, Claude Desktop, ...) GitDragon's git tools for the repositories you name. It is a separate binary
(`crates/mcp`) built on `git-core`, and it ships next to the GitDragon executable.

It speaks **stdio only**: the client starts it as a child process. It never opens a network port.

## Install

**From the app:** Settings > AI > *Install MCP server*. Enter the repository's absolute path, set the write tools
Off (default) or On, then pick Claude Code or Claude Desktop. A dialog shows the exact entry and the file it goes into; nothing
is written until you confirm.

- Claude Code: `~/.claude.json` (the same place `claude mcp add --scope user` writes).
- Claude Desktop: `%APPDATA%\Claude\claude_desktop_config.json` (Windows),
  `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS), `~/.config/Claude/...` (Linux).

The helper only adds or replaces `mcpServers.gitdragon`; other servers and settings stay as they were. The previous file
is copied to `<name>.gitdragon-bak` first, and a file that is not valid JSON is left untouched. Restart the client
afterwards.

**By hand (Claude Code):**

```sh
claude mcp add --scope user gitdragon -- /path/to/gitdragon-mcp --repo /abs/path/to/repo
```

**From source:** `cargo build --release -p mcp` builds `target/release/gitdragon-mcp`.

## Command line

```text
gitdragon-mcp --repo <ABSOLUTE PATH> [--repo <PATH>...] [--allow-writes]
```

| Option | Meaning |
|---|---|
| `--repo <PATH>` | A repository the tools may touch. Repeatable. Absolute paths only; each must be a git worktree. |
| `--allow-writes` | Also offer the write tools. Off by default. |
| `-h`, `--help` / `-V`, `--version` | Help / version (printed to stderr; stdout carries only JSON-RPC). |

With one `--repo`, tools may omit the `repo` parameter. With several, every call must pass `repo` (one of the allowed
paths, or its worktree root).

## Tools

Read tools (always available, marked read-only):

| Tool | What it returns |
|---|---|
| `status` | Branch, upstream ahead/behind, stash count, staged / unstaged / untracked files. |
| `log` | Short id, date, author, subject. `rev` (or range), `paths`, `limit` (default 30, max 500). |
| `diff` | Unified diff. `target`: `worktree` (default), `staged`, `commit` (+ `rev`), `range` (+ `from`, `to`). `paths` for worktree / staged. |
| `blame` | `git blame` of `path`, optionally at `rev` and for `start_line`..`end_line`. |
| `branches` | Local branches (current marked `*`, upstream ahead/behind) and, unless `remote: false`, remote-tracking ones. |
| `show` | One commit: id, parents, author, committer, message, changed files with line counts. |

Write tools (only with `--allow-writes`; otherwise not listed and not callable):

| Tool | What it does |
|---|---|
| `stage` / `unstage` | `git add` / reset the index for repo-relative `paths` (worktree changes are kept). |
| `commit` | Commits the staged changes with `message` (summary line, optional blank line + body). `amend` defaults to false. |
| `create_branch` | Creates `name` at `start` (default HEAD); `switch: true` also checks it out. |

There is no push, pull, reset, checkout of files, or raw git command.

## Safety

- **Allow-list:** only the `--repo` repositories are reachable; other paths are refused.
- **Writes off by default:** the write tools are removed from the server, and the methods refuse as well.
- **No argument passthrough:** revisions may not start with `-` or contain spaces / control characters; paths must be
  repo-relative (no absolute paths, no `..`). Up to 1,000 paths and a 64 KiB commit message per call.
- **No secrets in output:** every result and error goes through `git_core::redact` (tokens, credentials in URLs, ...)
  before it is capped at 100 KB, so a cut never leaves half a secret. `crates/mcp/tests/redaction.rs` checks this with
  canary tokens.
- **Hardened reads:** read tools run git with hooks and fsmonitor disabled and no optional locks, like the app's own
  background reads.
- Any future HTTP transport must bind to 127.0.0.1 and require a token and an Origin check.

## Troubleshooting

- *"gitdragon-mcp was not found next to the GitDragon executable"* (Settings): the binary must sit in the same folder as
  `gitdragon` / `gitdragon.exe`. Release packages include it; for a dev build run `cargo build -p mcp`.
- *"repository not allowed"*: pass one of the paths listed in the error, or add another `--repo`.
- The client lists no write tools: the entry was installed read-only; reinstall with the write tools On.
