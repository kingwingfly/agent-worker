//! A stand-in for the real shared MCP server, so the stack can be deployed and smoke
//! tested before the real one is wired in. Point `containers/mcp/Dockerfile` at your own
//! server to replace it.
//!
//! It runs behind `mcp-front`, which owns the container's open port. What the real server
//! has to match:
//! - MCP over streamable HTTP at `/mcp`, on the loopback address `mcp-front` forwards to;
//! - requests arrive with `Host: localhost:8080`, which rmcp accepts by default;
//! - the `x-agent-session` header names the calling session. The Worker sets it from the
//!   verified session token, so it can be trusted for per-session bookkeeping;
//! - a tool that makes a file saves it in the directory `mcp-front` serves and returns
//!   its path, which is how an agent on another machine gets hold of it.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

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

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SaveText {
    /// A name for the file, such as `note.txt`.
    name: String,
    /// What to put in the file.
    text: String,
}

#[derive(Clone)]
struct Example {
    /// Where `save_text` puts files, which is the directory `mcp-front` serves.
    files_dir: Arc<PathBuf>,
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Example {
    fn new(files_dir: Arc<PathBuf>) -> Self {
        Self {
            files_dir,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Returns the given text unchanged. Use it to check the connection.")]
    fn echo(&self, Parameters(Echo { text }): Parameters<Echo>) -> String {
        text
    }

    #[tool(
        description = "Saves text as a file on this server and returns the file's path. \
                       Use it to check that files can be fetched from the server."
    )]
    async fn save_text(
        &self,
        Parameters(SaveText { name, text }): Parameters<SaveText>,
    ) -> Result<String, String> {
        // A name, not a path: whatever directories it came with are dropped.
        let name = Path::new(&name)
            .file_name()
            .ok_or("the name must be a file name")?;
        let path = self.files_dir.join(name);
        let saved = async {
            tokio::fs::create_dir_all(&*self.files_dir).await?;
            tokio::fs::write(&path, text).await
        };
        saved.await.map_err(|error| error.to_string())?;
        Ok(path.display().to_string())
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
    // `mcp-front` starts this process and sets both: the address it forwards MCP requests
    // to, and the directory it serves files from. The fallbacks are for running alone.
    let listen: SocketAddr = match std::env::var("MCP_FRONT_MCP_ADDR") {
        Ok(addr) => addr.parse().map_err(std::io::Error::other)?,
        Err(_) => ([127, 0, 0, 1], 8081).into(),
    };
    let files_dir = Arc::new(
        std::env::var_os("MCP_FRONT_FILES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("mcp-files")),
    );
    let service = StreamableHttpService::new(
        move || Ok(Example::new(files_dir.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await
}
