use std::{
    env,
    ffi::OsString,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;

/// Where the MCP server and the file server listen unless told otherwise.
const MCP_PORT: u16 = 8081;
const FILES_PORT: u16 = 8082;

/// The variables that are also set for the MCP server.
pub const MCP_ADDR: &str = "MCP_FRONT_MCP_ADDR";
pub const FILES_DIR: &str = "MCP_FRONT_FILES_DIR";

#[derive(Debug, Clone)]
pub struct Config {
    /// The one address reachable from outside the container.
    pub listen: SocketAddr,
    /// Where the MCP server listens.
    pub mcp: SocketAddr,
    /// Where the file server listens. It has no authentication of its own, so this has
    /// to be a loopback address.
    pub files_listen: SocketAddr,
    /// The directory the MCP server saves files in, and the only one served.
    pub files_dir: PathBuf,
    /// How long the MCP server may take to start listening.
    pub start_timeout: Duration,
    /// The MCP server's command line. Empty when something else starts it.
    pub command: Vec<OsString>,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn addr(name: &str, default: SocketAddr) -> anyhow::Result<SocketAddr> {
    match var(name) {
        Some(value) => value.parse().with_context(|| name.to_owned()),
        None => Ok(default),
    }
}

impl Config {
    /// Reads the `MCP_FRONT_*` variables. `command` is everything on the command line.
    pub fn from_env(command: Vec<OsString>) -> anyhow::Result<Self> {
        let files_listen = addr(
            "MCP_FRONT_FILES_ADDR",
            (Ipv4Addr::LOCALHOST, FILES_PORT).into(),
        )?;
        anyhow::ensure!(
            files_listen.ip().is_loopback(),
            "MCP_FRONT_FILES_ADDR must be a loopback address, not {files_listen}"
        );
        Ok(Self {
            listen: addr(
                "MCP_FRONT_LISTEN",
                (Ipv4Addr::UNSPECIFIED, agent_proto::MCP_PORT).into(),
            )?,
            mcp: addr(MCP_ADDR, (Ipv4Addr::LOCALHOST, MCP_PORT).into())?,
            files_listen,
            files_dir: var(FILES_DIR)
                .map(PathBuf::from)
                .with_context(|| format!("{FILES_DIR} is not set"))?,
            start_timeout: Duration::from_secs(
                var("MCP_FRONT_START_TIMEOUT_SECS")
                    .map(|secs| secs.parse().context("MCP_FRONT_START_TIMEOUT_SECS"))
                    .transpose()?
                    .unwrap_or(60),
            ),
            command,
        })
    }
}
