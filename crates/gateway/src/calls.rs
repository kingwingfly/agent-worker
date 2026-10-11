//! What a container may ask of the Worker with its session token.
//!
//! A token says which chat a request comes from. The agent can read its own, and with it
//! anything it runs can call this Worker directly, from inside the container or from
//! anywhere else. So the token alone decides nothing here: a request also has to be one
//! this service makes, on behalf of a container that is still the chat's, within the
//! chat's budget.
//!
//! Nothing here touches the network, so it is tested on the host.

use serde::{Deserialize, Serialize};

/// Whether a request to `/llm` is one Claude Code makes for a turn: a message to the
/// model, or a count of the tokens one would take. `path` is what follows `/llm`.
///
/// The gateway's key opens whatever else the gateway offers, and that key belongs to
/// the operator and is shared by every chat.
pub fn is_model_call(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST")
        && matches!(
            path.trim_end_matches('/'),
            "/v1/messages" | "/v1/messages/count_tokens"
        )
}

#[derive(Deserialize)]
struct Named {
    model: Option<String>,
}

/// A model's name as Claude Code puts it in a request. A name can be configured with a
/// suffix in square brackets that tells Claude Code about the model's window; it is not
/// part of what is sent.
fn sent_as(configured: &str) -> &str {
    configured.split('[').next().unwrap_or(configured).trim()
}

/// The model the request in `body` is for, if it can be read and names one.
pub fn named_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Named>(body).ok()?.model
}

/// Whether the request in `body` names one of the deployment's two models. A token
/// budget only limits spending while it is spent on the models it was chosen for.
///
/// With either of the two left to Claude Code's default there is no telling what a
/// legitimate request names, and every model passes.
pub fn names_a_configured_model(
    body: &[u8],
    model: Option<&str>,
    small_model: Option<&str>,
) -> bool {
    let (Some(model), Some(small_model)) = (model, small_model) else {
        return true;
    };
    named_model(body).is_some_and(|named| {
        [model, small_model]
            .into_iter()
            .any(|own| sent_as(own) == named)
    })
}

/// What the proxy asks a chat's session before it acts on a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    /// The expiry the token carries, which is what tells one container's token from
    /// another's: each start signs a new one.
    pub expires_at: u64,
    /// The request is a model call, which the daily budget has to allow.
    pub spends: bool,
}

/// What the session knows that an admission is decided by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// The expiry of the token the chat's container was started with. `None` where
    /// containers are not started by the session, as with a runner started by hand.
    pub issued: Option<u64>,
    /// That container is running.
    pub running: bool,
    /// Tokens used today, and how many a day allows; zero allows any number.
    pub used: u64,
    pub budget: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The token is not the running container's: the container it was made for has
    /// been stopped or replaced.
    Stale,
    /// Today's tokens are used up.
    OverBudget,
}

impl Refusal {
    /// The status the session answers the proxy with.
    pub fn status(self) -> u16 {
        match self {
            Self::Stale => 401,
            Self::OverBudget => 402,
        }
    }

    pub fn from_status(status: u16) -> Option<Self> {
        [Self::Stale, Self::OverBudget]
            .into_iter()
            .find(|refusal| refusal.status() == status)
    }
}

/// Why a request is turned away although its token is valid, if it is.
pub fn refusal(asked: Admission, standing: Standing) -> Option<Refusal> {
    let current = standing
        .issued
        .is_none_or(|issued| standing.running && issued == asked.expires_at);
    if !current {
        Some(Refusal::Stale)
    } else if asked.spends && standing.budget > 0 && standing.used >= standing.budget {
        Some(Refusal::OverBudget)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_calls_claude_code_makes_for_a_turn_are_model_calls() {
        assert!(is_model_call("POST", "/v1/messages"));
        assert!(is_model_call("post", "/v1/messages/count_tokens"));
        assert!(is_model_call("POST", "/v1/messages/"));
        for (method, path) in [
            ("GET", "/v1/messages"),
            ("DELETE", "/v1/messages"),
            ("POST", "/v1/messages/batches"),
            ("GET", "/v1/models"),
            ("POST", "/v1/files"),
            ("POST", "/v1/chat/completions"),
            ("POST", "/"),
            ("POST", ""),
        ] {
            assert!(!is_model_call(method, path), "{method} {path}");
        }
    }

    #[test]
    fn a_request_has_to_name_one_of_the_two_configured_models() {
        let pinned =
            |body: &str| names_a_configured_model(body.as_bytes(), Some("big"), Some("small"));
        assert!(pinned(r#"{"model":"big","messages":[]}"#));
        assert!(pinned(r#"{"messages":[],"model":"small"}"#));
        assert!(!pinned(r#"{"model":"another","messages":[]}"#));
        assert!(!pinned(r#"{"model":"Big"}"#));
        // Nothing can be said for a request that names none, or cannot be read.
        assert!(!pinned(r#"{"messages":[]}"#));
        assert!(!pinned(r#"{"model":7}"#));
        assert!(!pinned("model=big"));
        assert!(!pinned(""));
    }

    #[test]
    fn a_configured_name_is_compared_as_claude_code_sends_it() {
        let body = br#"{"model":"big"}"#;
        assert!(names_a_configured_model(
            body,
            Some("big[1m]"),
            Some("small")
        ));
        assert!(names_a_configured_model(body, Some("small"), Some(" big ")));
    }

    #[test]
    fn no_model_is_refused_while_one_is_left_to_the_default() {
        let body = br#"{"model":"whatever"}"#;
        assert!(names_a_configured_model(body, Some("big"), None));
        assert!(names_a_configured_model(body, None, Some("small")));
        assert!(names_a_configured_model(body, None, None));
    }

    const STANDING: Standing = Standing {
        issued: Some(5_000),
        running: true,
        used: 10,
        budget: 100,
    };
    const CALL: Admission = Admission {
        expires_at: 5_000,
        spends: true,
    };

    #[test]
    fn the_running_containers_token_is_admitted_within_the_budget() {
        assert_eq!(refusal(CALL, STANDING), None);
        let unlimited = Standing {
            used: u64::MAX,
            budget: 0,
            ..STANDING
        };
        assert_eq!(refusal(CALL, unlimited), None);
    }

    #[test]
    fn a_token_does_not_outlive_its_container() {
        let earlier = Admission {
            expires_at: 4_000,
            ..CALL
        };
        assert_eq!(refusal(earlier, STANDING), Some(Refusal::Stale));
        let stopped = Standing {
            running: false,
            ..STANDING
        };
        assert_eq!(refusal(CALL, stopped), Some(Refusal::Stale));
        // A session that has never started a container has issued nothing.
        let never = Standing {
            issued: Some(0),
            ..STANDING
        };
        assert_eq!(refusal(CALL, never), Some(Refusal::Stale));
    }

    #[test]
    fn a_runner_started_by_hand_has_no_container_to_be_checked_against() {
        let by_hand = Standing {
            issued: None,
            running: false,
            ..STANDING
        };
        assert_eq!(refusal(CALL, by_hand), None);
    }

    #[test]
    fn a_used_up_budget_stops_model_calls_and_nothing_else() {
        let spent = Standing {
            used: 100,
            ..STANDING
        };
        assert_eq!(refusal(CALL, spent), Some(Refusal::OverBudget));
        let other = Admission {
            spends: false,
            ..CALL
        };
        assert_eq!(refusal(other, spent), None);
    }

    #[test]
    fn a_refusal_survives_the_trip_as_a_status() {
        for refusal in [Refusal::Stale, Refusal::OverBudget] {
            assert_eq!(Refusal::from_status(refusal.status()), Some(refusal));
        }
        assert_eq!(Refusal::from_status(200), None);
        assert_eq!(Refusal::from_status(500), None);
    }
}
