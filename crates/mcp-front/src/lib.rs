//! What stands in front of the shared MCP server inside its container.
//!
//! The MCP server and the agents do not share a disk, so a file the MCP server saves has
//! to travel over the network to be of use to an agent. This process starts the MCP
//! server and puts two things beside it:
//!
//! - a file server ([`files`]) that hands out what the MCP server saved, and
//! - a pingora proxy ([`proxy`]) on the container's one open port, which sends `/file`
//!   to the file server and everything else to the MCP server.
//!
//! Both servers behind the proxy listen on loopback only.

pub mod config;
pub mod files;
pub mod proxy;
pub mod supervise;

use std::{net::TcpListener, sync::Mutex};

use anyhow::Context;
use async_trait::async_trait;
use pingora::{
    proxy::http_proxy_service,
    server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch, ShutdownWatch},
    services::background::{BackgroundService, background_service},
};
use tokio::signal::unix::{SignalKind, signal};

pub use config::Config;

use crate::{proxy::Front, supervise::Child};

/// The file server, run by pingora next to the proxy.
struct FileServer {
    /// Bound before anything runs, so that a taken port is an error at start.
    listener: Mutex<Option<TcpListener>>,
    router: axum::Router,
}

#[async_trait]
impl BackgroundService for FileServer {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let Some(listener) = self.listener.lock().expect("not poisoned").take() else {
            return;
        };
        let served = async {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            axum::serve(listener, self.router.clone())
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await
        };
        if let Err(error) = served.await {
            tracing::error!(%error, "the file server stopped");
        }
    }
}

/// Ends the servers when the container is told to stop, or when the MCP server is gone.
/// Either way there is nothing worth waiting for: in-flight requests are dropped.
struct Stop {
    mcp_exit: supervise::Exit,
}

#[async_trait]
impl ShutdownSignalWatch for Stop {
    async fn recv(&self) -> ShutdownSignal {
        let mut mcp_exit = self.mcp_exit.clone();
        let mut terminate = signal(SignalKind::terminate()).expect("signals are available");
        let mut interrupt = signal(SignalKind::interrupt()).expect("signals are available");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = mcp_exit.wait_for(Option::is_some) => {}
        }
        ShutdownSignal::FastShutdown
    }
}

/// Runs until told to stop. An error means the MCP server could not be started or went
/// away by itself.
pub fn run(cfg: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.files_dir)
        .with_context(|| format!("creating {}", cfg.files_dir.display()))?;
    let router = files::router(&cfg.files_dir)?;
    let files_listener = TcpListener::bind(cfg.files_listen)
        .with_context(|| format!("the file server cannot listen on {}", cfg.files_listen))?;
    files_listener.set_nonblocking(true)?;

    // An MCP server can read from these where to listen and where to save files, instead
    // of being told twice.
    let child = Child::spawn(
        &cfg.command,
        &[
            (config::MCP_ADDR, cfg.mcp.to_string().into()),
            (config::FILES_DIR, cfg.files_dir.clone().into()),
        ],
    )?;
    // Callers take an open port to mean the MCP server is there, so it is opened last.
    if let Err(error) = child.wait_until_listening(cfg.mcp, cfg.start_timeout) {
        child.stop();
        return Err(error);
    }

    let mut server = Server::new(None).context("setting up the proxy")?;
    server.bootstrap();
    let front = Front {
        mcp: cfg.mcp,
        files: cfg.files_listen,
    };
    let mut proxy = http_proxy_service(&server.configuration, front);
    proxy.add_tcp(&cfg.listen.to_string());
    server.add_service(proxy);
    server.add_service(background_service(
        "file server",
        FileServer {
            listener: Mutex::new(Some(files_listener)),
            router,
        },
    ));
    tracing::info!(
        listen = %cfg.listen, mcp = %cfg.mcp, files = %cfg.files_listen,
        dir = %cfg.files_dir.display(), "serving"
    );
    server.run(RunArgs {
        shutdown_signal: Box::new(Stop {
            mcp_exit: child.exit(),
        }),
    });

    let exited = child.exited();
    child.stop();
    match exited {
        Some(how) => anyhow::bail!("the MCP server stopped ({how})"),
        None => Ok(()),
    }
}
