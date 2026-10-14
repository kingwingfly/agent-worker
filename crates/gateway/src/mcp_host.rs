//! The one Durable Object in front of the shared MCP container. It starts the container
//! on the first request, forwards requests to it and stops it once nobody has used it for
//! a while.

use std::{cell::Cell, time::Duration};

use agent_proto::MCP_PORT;
use worker::{
    Delay, DurableObject, Env, Request, Response, Result, State, console_error, durable_object,
};

use crate::{config::Config, container_ext, platform::error};

pub const MCP_BINDING: &str = "MCP_HOST";
/// Every agent talks to the same instance.
pub const MCP_INSTANCE: &str = "shared";
/// Requests to this path start the container and keep it up without being forwarded.
/// Sessions call it when a turn begins, so the cold start overlaps the first model call.
pub const WARM_PATH: &str = "/__warm";

const READY_POLL_MS: u64 = 200;
const READY_ATTEMPTS: u32 = 150;
const BUSY_RECHECK_MS: u64 = 60_000;

#[durable_object]
pub struct McpHostDO {
    state: State,
    env: Env,
    /// The server has answered on its port since this object was loaded.
    ready: Cell<bool>,
    /// Requests whose response headers have not arrived yet.
    in_flight: Cell<u32>,
}

impl McpHostDO {
    async fn ensure_ready(&self, cfg: &Config) -> Result<worker::Fetcher> {
        let container = self
            .state
            .container()
            .ok_or_else(|| error("McpHostDO has no container"))?;
        if !container.running() {
            self.ready.set(false);
            // Read at every start, so a secret that was changed is in the next container.
            // A missing one leaves the container stopped: an MCP server without its
            // credentials would only fail later, at the first tool call.
            let vars = cfg.mcp_container_vars(&self.env)?;
            let vars: Vec<(&str, &str)> = vars
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            container_ext::start(&container, &vars)?;
        }
        let port = container.get_tcp_port(MCP_PORT)?;
        if self.ready.get() {
            return Ok(port);
        }
        let mut last = String::new();
        for _ in 0..READY_ATTEMPTS {
            // Any HTTP answer, even an error status, means the server is listening.
            let probe = format!("http://localhost:{MCP_PORT}/");
            match port.fetch(probe, None).await {
                Ok(_) => {
                    self.ready.set(true);
                    return Ok(port);
                }
                Err(error) => last = error.to_string(),
            }
            Delay::from(Duration::from_millis(READY_POLL_MS)).await;
        }
        Err(error(format!(
            "the MCP server did not start listening ({last})"
        )))
    }
}

impl DurableObject for McpHostDO {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            ready: Cell::new(false),
            in_flight: Cell::new(0),
        }
    }

    async fn fetch(&self, request: Request) -> Result<Response> {
        let cfg = Config::load(&self.env)?;
        let port = match self.ensure_ready(&cfg).await {
            Ok(port) => port,
            Err(error) => {
                console_error!("MCP container: {error}");
                return Response::error("the MCP server is unavailable", 503);
            }
        };
        // Each request pushes the shutdown out by the idle time.
        self.state
            .storage()
            .set_alarm(Duration::from_millis(cfg.mcp_idle_ttl_ms))
            .await?;
        if request.path() == WARM_PATH {
            return Response::empty();
        }

        self.in_flight.set(self.in_flight.get() + 1);
        let response = port.fetch_request(request).await;
        self.in_flight.set(self.in_flight.get() - 1);
        response
    }

    async fn alarm(&self) -> Result<Response> {
        if self.in_flight.get() > 0 {
            self.state
                .storage()
                .set_alarm(Duration::from_millis(BUSY_RECHECK_MS))
                .await?;
        } else if let Some(container) = self.state.container()
            && container.running()
        {
            self.ready.set(false);
            container.destroy(None).await?;
        }
        Response::empty()
    }
}
