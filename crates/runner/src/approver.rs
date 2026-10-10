//! The MCP tool Claude calls instead of showing a permission prompt
//! (`--permission-prompt-tool`). It parks the call until the user has answered through
//! the Durable Object, however long that takes.

use std::sync::Arc;

use agent_proto::Decision;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};

use crate::session::SessionHandle;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PermissionPrompt {
    /// Name of the tool Claude wants to run.
    pub tool_name: String,
    /// Arguments Claude wants to run it with.
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Clone)]
pub struct Approver {
    session: SessionHandle,
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

/// The JSON Claude expects back from a permission prompt tool.
pub fn claude_decision(decision: Decision, requested_input: Value) -> Value {
    match decision {
        Decision::Allow { updated_input, .. } => json!({
            "behavior": "allow",
            "updatedInput": updated_input.unwrap_or(requested_input),
        }),
        Decision::Deny { message } => json!({ "behavior": "deny", "message": message }),
    }
}

#[tool_router]
impl Approver {
    pub fn new(session: SessionHandle) -> Self {
        Self {
            session,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Ask the user whether a tool call may run.")]
    async fn permission_prompt(&self, Parameters(prompt): Parameters<PermissionPrompt>) -> String {
        let decision = self
            .session
            .request_permission(prompt.tool_name, prompt.input.clone())
            .await;
        claude_decision(decision, prompt.input).to_string()
    }
}

#[tool_handler]
impl ServerHandler for Approver {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

pub fn router(session: SessionHandle) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(Approver::new(session.clone())),
        Arc::new(LocalSessionManager::default()),
        // The default config only accepts loopback `Host` headers, which is what we want.
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new().nest_service("/mcp", service)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_echoes_the_requested_input_unless_replaced() {
        let requested = json!({ "command": "ls" });
        let allow = |updated_input| Decision::Allow { updated_input };
        assert_eq!(
            claude_decision(allow(None), requested.clone()),
            json!({ "behavior": "allow", "updatedInput": { "command": "ls" } })
        );
        assert_eq!(
            claude_decision(allow(Some(json!({ "command": "ls -l" }))), requested),
            json!({ "behavior": "allow", "updatedInput": { "command": "ls -l" } })
        );
    }

    #[test]
    fn deny_carries_the_reason() {
        let deny = Decision::Deny {
            message: "no".into(),
        };
        assert_eq!(
            claude_decision(deny, Value::Null),
            json!({ "behavior": "deny", "message": "no" })
        );
    }
}
