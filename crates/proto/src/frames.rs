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
    UserMessage {
        turn_id: String,
        text: String,
        /// The level to answer at. `None` leaves it to the CLI, which has a default for
        /// each model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
    },
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

/// How much work the model puts into an answer. The names are the CLI's own, for
/// `--effort` and for its `effortLevel` setting. It ignores a name it does not know
/// without saying so, so they have to stay exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// Every level, from the least work to the most.
    pub const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The level called `name`, however it is capitalised.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(name))
    }
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
    fn a_user_message_names_an_effort_only_when_one_is_set() {
        let message = |effort| ToRunner::UserMessage {
            turn_id: "t".into(),
            text: "hi".into(),
            effort,
        };
        let plain = json!({ "type": "user_message", "turn_id": "t", "text": "hi" });
        assert_eq!(serde_json::to_value(message(None)).unwrap(), plain);
        // Which is also all that a Worker from before there were levels sends.
        assert_eq!(
            serde_json::from_value::<ToRunner>(plain).unwrap(),
            message(None)
        );
        let high = serde_json::to_value(message(Some(Effort::High))).unwrap();
        assert_eq!(high["effort"], "high");
        assert_eq!(
            serde_json::from_value::<ToRunner>(high).unwrap(),
            message(Some(Effort::High))
        );
    }

    #[test]
    fn effort_levels_are_spelled_in_frames_as_on_the_command_line() {
        for level in Effort::ALL {
            assert_eq!(serde_json::to_value(level).unwrap(), level.as_str());
            assert_eq!(Effort::parse(&level.as_str().to_uppercase()), Some(level));
        }
        assert_eq!(Effort::parse("auto"), None);
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
