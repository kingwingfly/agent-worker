//! JSON frames exchanged over the WebSocket between a `SessionDO` and its runner.
//!
//! Frames the runner emits carry a monotonically increasing `seq`. The runner keeps them
//! buffered until the Durable Object acknowledges them, so a Durable Object that was
//! evicted mid-turn can reconnect and ask for a replay.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Durable Object → runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToRunner {
    /// Start a turn. Ignored if `turn_id` is the turn already running or last finished.
    UserMessage { turn_id: String, text: String },
    /// Answer to a [`FromRunner::PermissionRequest`].
    PermissionDecision {
        request_id: String,
        decision: Decision,
    },
    /// Stop the running turn.
    Interrupt,
    /// Everything up to and including `seq` is durably stored and may be dropped.
    Ack { seq: u64 },
    /// Replay every buffered frame with a sequence number greater than `after_seq`.
    Resume { after_seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum Decision {
    Allow {
        /// Replacement tool input. `None` runs the tool with the input Claude asked for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
    },
    Deny {
        /// Shown to Claude as the reason.
        message: String,
    },
}

/// Runner → Durable Object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromRunner {
    /// First frame on every connection.
    Hello {
        runner_version: String,
        /// Highest sequence number the runner has emitted so far.
        last_seq: u64,
        /// Turn in progress, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        running_turn: Option<String>,
    },
    /// One line of `claude --output-format stream-json`, forwarded untouched.
    Event {
        seq: u64,
        turn_id: String,
        event: Value,
    },
    /// Claude wants to run a tool nothing pre-approved, or is asking the user a question
    /// (`tool_name == "AskUserQuestion"`).
    PermissionRequest {
        seq: u64,
        turn_id: String,
        request_id: String,
        tool_name: String,
        input: Value,
    },
    TurnDone {
        seq: u64,
        turn_id: String,
        result: TurnResult,
    },
    /// Keeps the connection, and so the Durable Object, alive between events.
    Heartbeat,
}

impl FromRunner {
    /// Sequence number, for the frames that are buffered and replayed.
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Event { seq, .. }
            | Self::PermissionRequest { seq, .. }
            | Self::TurnDone { seq, .. } => Some(*seq),
            Self::Hello { .. } | Self::Heartbeat => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnResult {
    pub is_error: bool,
    /// `subtype` of Claude's `result` message (`success`, `error_max_turns`, ...), or
    /// `runner_error` when the process died before producing one.
    pub subtype: String,
    /// Final answer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    /// Files created or changed under the output directory during this turn.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.billable() + self.cache_read_input_tokens
    }

    /// Tokens that count against a budget: everything except cache reads, which cost a
    /// fraction of the rest and would otherwise dominate the figure.
    pub fn billable(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_input_tokens
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Path relative to the output directory, `/`-separated.
    pub path: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_use_snake_case_type_tags() {
        let frame = ToRunner::PermissionDecision {
            request_id: "r1".into(),
            decision: Decision::Allow {
                updated_input: Some(json!({ "command": "ls" })),
            },
        };
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            json!({
                "type": "permission_decision",
                "request_id": "r1",
                "decision": { "behavior": "allow", "updated_input": { "command": "ls" } }
            })
        );
    }

    #[test]
    fn allow_defaults_when_fields_are_missing() {
        let decision: Decision = serde_json::from_value(json!({ "behavior": "allow" })).unwrap();
        assert_eq!(
            decision,
            Decision::Allow {
                updated_input: None
            }
        );
    }

    #[test]
    fn only_replayable_frames_have_a_seq() {
        assert_eq!(FromRunner::Heartbeat.seq(), None);
        let event = FromRunner::Event {
            seq: 7,
            turn_id: "t".into(),
            event: json!({}),
        };
        assert_eq!(event.seq(), Some(7));
    }
}
