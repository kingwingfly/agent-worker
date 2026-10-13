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

use std::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    thread,
};

use anyhow::Context;
use async_trait::async_trait;
use pingora::{
    proxy::http_proxy_service,
    server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch, ShutdownWatch},
    services::background::{BackgroundService, background_service},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

pub use config::Config;

use crate::{proxy::Front, supervise::Child};

/// Whether this process has been told to stop.
pub type Asked = watch::Receiver<bool>;

/// Starts watching for the signals a container is stopped by, and returns where to
/// look for them.
///
/// A signal nothing watches for ends the process there and then, with the MCP server
/// left running. So this comes before the MCP server is started, and that takes a
/// thread with a runtime of its own: the servers' runtimes belong to pingora, which only
/// starts looking for signals once its port is open.
fn watch_for_stop() -> anyhow::Result<Asked> {
    let (asked_sender, asked) = watch::channel(false);
    let (watching_sender, watching) = mpsc::channel();
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return drop(watching_sender.send(Err(error))),
            };
            runtime.block_on(async move {
                let signals = signal(SignalKind::terminate())
                    .and_then(|terminate| Ok((terminate, signal(SignalKind::interrupt())?)));
                let (mut terminate, mut interrupt) = match signals {
                    Ok(signals) => signals,
                    Err(error) => return drop(watching_sender.send(Err(error))),
                };
                let _ = watching_sender.send(Ok(()));
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
                // Stays said after this thread is gone: a receiver keeps the last value.
                let _ = asked_sender.send(true);
            });
        })?;
    watching
        .recv()
        .context("the thread that watches for signals stopped")?
        .context("watching for signals")?;
    Ok(asked)
}

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
    asked: Asked,
}

#[async_trait]
impl ShutdownSignalWatch for Stop {
    async fn recv(&self) -> ShutdownSignal {
        let mut mcp_exit = self.mcp_exit.clone();
        let mut asked = self.asked.clone();
        tokio::select! {
            _ = asked.wait_for(|asked| *asked) => {}
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
    // From before there is an MCP server to leave behind.
    let asked = watch_for_stop()?;

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
    match child.wait_until_listening(cfg.mcp, cfg.start_timeout, &asked) {
        Ok(true) => {}
        // Told to stop before there was anything to serve.
        Ok(false) => {
            child.stop();
            return Ok(());
        }
        Err(error) => {
            child.stop();
            return Err(error);
        }
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
            asked,
        }),
    });

    let exited = child.exited();
    child.stop();
    match exited {
        Some(how) => anyhow::bail!("the MCP server stopped ({how})"),
        None => Ok(()),
    }
}
