//! A stand-in for the real shared MCP server, so the stack can be deployed and smoke
//! tested before the real one is wired in. Point `containers/mcp/Dockerfile` at your own
//! server to replace it.
//!
//! What the real server has to match:
//! - MCP over streamable HTTP at `/mcp`, port 8080, on all interfaces;
//! - requests arrive with `Host: localhost:8080`, which rmcp accepts by default;
//! - the `x-agent-session` header names the calling session. The Worker sets it from the
//!   verified session token, so it can be trusted for per-session bookkeeping.

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Echo {
    /// Text to send back.
    text: String,
}

#[derive(Clone)]
struct Example {
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Example {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Returns the given text unchanged. Use it to check the connection.")]
    fn echo(&self, Parameters(Echo { text }): Parameters<Echo>) -> String {
        text
    }
}

#[tool_handler]
impl ServerHandler for Example {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let service = StreamableHttpService::new(
        || Ok(Example::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", agent_proto::MCP_PORT)).await?;
    axum::serve(listener, app).await
}
