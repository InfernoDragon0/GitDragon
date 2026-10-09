//! MCP wiring: one `#[tool]` per [`GitTools`] method. Write tools are removed from the router
//! (not listed, not callable) unless `--allow-writes` was given; the methods refuse too.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::tools::{
    BlameParams, BranchesParams, CommitParams, CreateBranchParams, DiffParams, GitTools, LogParams,
    PathsParams, RepoParams, ShowParams, ToolResult, WRITE_TOOLS,
};

#[derive(Clone)]
pub struct GitServer {
    tools: Arc<GitTools>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl GitServer {
    pub fn new(tools: GitTools) -> Self {
        let mut tool_router = Self::tool_router();
        if !tools.allow_writes() {
            for name in WRITE_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self {
            tools: Arc::new(tools),
            tool_router,
        }
    }

    /// Names of the tools this server offers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.into_owned())
            .collect()
    }

    #[tool(
        description = "Working tree status: branch, upstream ahead/behind, staged/unstaged/untracked files.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self, Parameters(p): Parameters<RepoParams>) -> ToolResult {
        self.tools.status(p).await
    }

    #[tool(
        description = "Commit history (short id, date, author, subject). Optional rev/range, paths filter and limit (default 30, max 500).",
        annotations(read_only_hint = true)
    )]
    async fn log(&self, Parameters(p): Parameters<LogParams>) -> ToolResult {
        self.tools.log(p).await
    }

    #[tool(
        description = "Unified diff. target: worktree (unstaged, default), staged, commit (needs rev) or range (needs from and to). Large diffs are truncated; narrow with paths.",
        annotations(read_only_hint = true)
    )]
    async fn diff(&self, Parameters(p): Parameters<DiffParams>) -> ToolResult {
        self.tools.diff(p).await
    }

    #[tool(
        description = "Line-by-line blame of a file, optionally at a revision and for a line range.",
        annotations(read_only_hint = true)
    )]
    async fn blame(&self, Parameters(p): Parameters<BlameParams>) -> ToolResult {
        self.tools.blame(p).await
    }

    #[tool(
        description = "Local branches (current marked *, upstream with ahead/behind) and remote-tracking branches.",
        annotations(read_only_hint = true)
    )]
    async fn branches(&self, Parameters(p): Parameters<BranchesParams>) -> ToolResult {
        self.tools.branches(p).await
    }

    #[tool(
        description = "One commit: id, parents, author, committer, message and changed files with line counts.",
        annotations(read_only_hint = true)
    )]
    async fn show(&self, Parameters(p): Parameters<ShowParams>) -> ToolResult {
        self.tools.show(p).await
    }

    #[tool(
        description = "Stage files (git add) by repo-relative path. Needs --allow-writes.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn stage(&self, Parameters(p): Parameters<PathsParams>) -> ToolResult {
        self.tools.stage(p).await
    }

    #[tool(
        description = "Unstage files (keeps worktree changes) by repo-relative path. Needs --allow-writes.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn unstage(&self, Parameters(p): Parameters<PathsParams>) -> ToolResult {
        self.tools.unstage(p).await
    }

    #[tool(
        description = "Commit the staged changes with a message. amend defaults to false. Needs --allow-writes.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn commit(&self, Parameters(p): Parameters<CommitParams>) -> ToolResult {
        self.tools.commit(p).await
    }

    #[tool(
        description = "Create a branch at start (default HEAD); switch=true also checks it out. Needs --allow-writes.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn create_branch(&self, Parameters(p): Parameters<CreateBranchParams>) -> ToolResult {
        self.tools.create_branch(p).await
    }
}

// The field, not a fresh `Self::tool_router()` (the macro default), so removed routes stay gone.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for GitServer {
    fn get_info(&self) -> ServerConfig {
        let writes = if self.tools.allow_writes() {
            "Write tools (stage, unstage, commit, create_branch) are enabled."
        } else {
            "Read-only: write tools are disabled."
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "gitdragon-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Git tools for the repositories GitDragon allowed. Pass `repo` (absolute path) \
                 when more than one is allowed. Paths are repo-relative. {writes}"
            ))
    }
}
