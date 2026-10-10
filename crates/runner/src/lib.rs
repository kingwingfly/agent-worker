//! In-container supervisor for one agent session.
//!
//! It launches the unmodified `claude` CLI, relays its stream-json output to the session's
//! Durable Object over a WebSocket, turns Claude's permission prompts into requests the
//! user answers remotely, and packs up the state that has to survive the container.

pub mod approver;
pub mod claude;
pub mod config;
pub mod http;
pub mod session;
pub mod state;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use tokio::{net::TcpListener, task::JoinSet};

pub use config::Config;
pub use session::SessionHandle;

/// A started runner. Dropping it stops the servers.
pub struct Runner {
    pub addr: SocketAddr,
    pub approver_addr: SocketAddr,
    pub session: SessionHandle,
    servers: JoinSet<std::io::Result<()>>,
}

impl Runner {
    /// Resolves when either server stops, which is only ever a failure.
    pub async fn wait(mut self) -> anyhow::Result<()> {
        match self.servers.join_next().await {
            Some(Ok(Ok(()))) | None => anyhow::bail!("server stopped"),
            Some(Ok(Err(error))) => Err(error).context("server failed"),
            Some(Err(error)) => Err(error).context("server task failed"),
        }
    }
}

pub async fn start(cfg: Config) -> anyhow::Result<Runner> {
    for dir in cfg.dirs_to_create() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let listener = TcpListener::bind(cfg.listen)
        .await
        .context("binding control port")?;
    let approver_listener = TcpListener::bind(cfg.approver_listen)
        .await
        .context("binding approver port")?;
    let addr = listener.local_addr()?;
    let approver_addr = approver_listener.local_addr()?;

    let mcp_config = claude::mcp_config(&cfg, &format!("http://{approver_addr}/mcp"));
    std::fs::write(
        cfg.mcp_config_path(),
        serde_json::to_vec_pretty(&mcp_config)?,
    )
    .context("writing MCP config")?;

    let cfg = Arc::new(cfg);
    let session = session::spawn(cfg.clone());
    let mut servers = JoinSet::new();
    let control = http::router(cfg, session.clone());
    servers.spawn(async move { axum::serve(listener, control).await });
    let approver = approver::router(session.clone());
    servers.spawn(async move { axum::serve(approver_listener, approver).await });

    Ok(Runner {
        addr,
        approver_addr,
        session,
        servers,
    })
}

/// Stops other processes of the same user, i.e. the agent, from reading this process's
/// memory or environment through ptrace or `/proc`.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    if let Err(error) = nix::sys::prctl::set_dumpable(false) {
        tracing::warn!(%error, "could not make the runner non-dumpable");
    }
}
