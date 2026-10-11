//! The MCP server as a child process. This process is the container's entry point, so
//! the two live and die together: nothing is served before the MCP server listens, and
//! when it goes away so does everything else, which stops the container.

use std::{
    ffi::OsString,
    net::{SocketAddr, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use tokio::{
    process::Command,
    sync::{oneshot, watch},
};

/// How the MCP server ended, once it has.
pub type Exit = watch::Receiver<Option<String>>;

pub struct Child {
    exit: Exit,
    /// Kept so the channel stays open when there is no child to close it.
    _exit_sender: Option<watch::Sender<Option<String>>>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Child {
    /// Starts `command` with `env` added to its environment. With an empty command there
    /// is nothing to supervise: something else runs the MCP server.
    pub fn spawn(command: &[OsString], env: &[(&str, OsString)]) -> anyhow::Result<Self> {
        let (exit_sender, exit) = watch::channel(None);
        let Some((program, args)) = command.split_first() else {
            return Ok(Self {
                exit,
                _exit_sender: Some(exit_sender),
                stop: None,
                thread: None,
            });
        };

        let (stop, stopped) = oneshot::channel();
        let (started_sender, started) = mpsc::channel();
        let mut process = Command::new(program);
        process.args(args).kill_on_drop(true);
        for (name, value) in env {
            process.env(name, value);
        }
        // A thread of its own, because the servers' runtimes belong to pingora and come
        // and go with it.
        let thread = thread::Builder::new()
            .name("mcp-server".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => return drop(started_sender.send(Err(error))),
                };
                runtime.block_on(async move {
                    let mut child = match process.spawn() {
                        Ok(child) => child,
                        Err(error) => return drop(started_sender.send(Err(error))),
                    };
                    let _ = started_sender.send(Ok(()));
                    tokio::select! {
                        status = child.wait() => {
                            let how = match status {
                                Ok(status) => status.to_string(),
                                Err(error) => error.to_string(),
                            };
                            let _ = exit_sender.send(Some(how));
                        }
                        _ = stopped => {
                            let _ = child.kill().await;
                        }
                    }
                });
            })?;
        started
            .recv()
            .context("the MCP server's thread stopped")?
            .with_context(|| format!("starting {}", program.to_string_lossy()))?;
        Ok(Self {
            exit,
            _exit_sender: None,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    pub fn exit(&self) -> Exit {
        self.exit.clone()
    }

    /// How the MCP server ended, if it has.
    pub fn exited(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    /// Blocks until something accepts connections on `addr`. Fails if the MCP server
    /// ends first or takes longer than `timeout`.
    pub fn wait_until_listening(&self, addr: SocketAddr, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(how) = self.exited() {
                anyhow::bail!("the MCP server stopped before it was listening ({how})");
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "nothing was listening on {addr} after {} seconds",
                timeout.as_secs()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Ends the MCP server if it is still running.
    pub fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
