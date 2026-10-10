//! Wire types shared by the gateway Worker (wasm) and the in-container runner (native).

pub mod frames;
pub mod token;

pub use frames::{Artifact, Decision, FromRunner, ToRunner, TurnResult, Usage};

/// Name of Claude's clarifying-question tool. It reaches the permission prompt like any
/// other tool, with the questions as its input.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// Port the runner listens on inside the agent container.
pub const RUNNER_PORT: u16 = 8080;
/// Port the shared MCP server listens on inside its container.
pub const MCP_PORT: u16 = 8080;

/// Environment variables the Durable Object passes to the agent container at start.
pub mod env {
    /// Public origin of the Worker, e.g. `https://agent.example.workers.dev`.
    pub const WORKER_ORIGIN: &str = "AGENT_WORKER_ORIGIN";
    /// Short-lived session token, see [`crate::token`].
    pub const SESSION_TOKEN: &str = "AGENT_SESSION_TOKEN";
    /// UUID used as the Claude session id.
    pub const CLAUDE_SESSION_ID: &str = "AGENT_CLAUDE_SESSION_ID";
    /// Model id as the LLM gateway names it.
    pub const MODEL: &str = "AGENT_MODEL";
    /// Model id for background requests (titles, summaries).
    pub const SMALL_MODEL: &str = "AGENT_SMALL_MODEL";
}
