//! What an "Always" answer to a permission prompt covers.
//!
//! The session keeps these answers and replies from them, so they last until `/new`
//! however often the container is replaced. The prompt's wording is derived from the same
//! rules, so it cannot promise something else.

use agent_proto::ASK_USER_QUESTION;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every request is put to the user.
    Nothing,
    /// The identical shell command. A prefix would let `git status; curl ... | sh` ride
    /// on an approval given for `git status`.
    ExactCommand,
    /// Every later call of the same tool, whatever its input.
    Tool,
}

pub fn scope(tool_name: &str) -> Scope {
    match tool_name {
        // An answer to one question says nothing about the next.
        ASK_USER_QUESTION => Scope::Nothing,
        "Bash" => Scope::ExactCommand,
        _ => Scope::Tool,
    }
}

/// Key an "Always" answer is stored under, or `None` if this request must be put to the
/// user every time.
pub fn key(tool_name: &str, input: &Value) -> Option<String> {
    match scope(tool_name) {
        Scope::Nothing => None,
        Scope::ExactCommand => {
            let command = input.get("command")?.as_str()?;
            Some(format!("{tool_name}\0{command}"))
        }
        Scope::Tool => Some(tool_name.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_is_remembered_per_exact_command() {
        let status = key("Bash", &json!({ "command": "git status" }));
        let chained = key("Bash", &json!({ "command": "git status; rm -rf /" }));
        assert!(status.is_some());
        assert_ne!(status, chained);
    }

    #[test]
    fn other_tools_are_remembered_by_name() {
        assert_eq!(
            key("mcp__domain__lookup", &json!({ "id": 1 })),
            key("mcp__domain__lookup", &json!({ "id": 2 })),
        );
        assert_ne!(key("Write", &json!({})), key("Edit", &json!({})));
    }

    #[test]
    fn questions_and_malformed_bash_are_never_remembered() {
        assert_eq!(key(ASK_USER_QUESTION, &json!({})), None);
        assert_eq!(key("Bash", &json!({})), None);
    }
}
