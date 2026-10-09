//! `gitdragon-mcp`: stdio MCP server. stdout carries only JSON-RPC; messages go to stderr.

use rmcp::ServiceExt as _;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    git_core::redact::install_panic_hook();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gitdragon-mcp: {}", git_core::redact::redact(&err));
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let config = match mcp::parse_args(std::env::args().skip(1))? {
        mcp::Command::Serve(config) => config,
        mcp::Command::Help => {
            eprint!("{}", mcp::USAGE);
            return Ok(());
        }
        mcp::Command::Version => {
            eprintln!("gitdragon-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };
    let runner = git_core::GitRunner::detect().map_err(|e| e.to_string())?;
    let tools = mcp::GitTools::open(&config, runner).await?;
    let service = mcp::GitServer::new(tools)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("could not start: {e}"))?;
    service
        .waiting()
        .await
        .map_err(|e| format!("server stopped: {e}"))?;
    Ok(())
}
