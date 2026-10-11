use agent_mcp_front::Config;
use tracing_subscriber::EnvFilter;

/// `mcp-front COMMAND [ARGS...]` runs the MCP server given as the command; see the
/// library for what it puts in front of it.
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let command = std::env::args_os().skip(1).collect();
    agent_mcp_front::run(Config::from_env(command)?)
}
