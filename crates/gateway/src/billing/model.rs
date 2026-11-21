//! Reserve tokens before an upstream call, then settle from its actual usage.
use std::time::Duration;

use agent_proto::Usage;
use serde_json::{Value, json};
use worker::{Delay, Env, Fetch, Headers, Method, Response, Result, console_error};

use super::post;
use crate::{
    platform::{error, expect_ok, random_uuid},
    proxy::{failure, kept_headers, rebuilt, refused},
};

pub enum Admission {
    Ready(Option<Reservation>),
    Refused(Response),
}

pub struct Reservation {
    env: Env,
    id: String,
}

impl Reservation {
    /// A repeated settlement has the same effect if the ledger committed but its response was lost.
    pub async fn finish(self, usage: Option<Usage>) {
        let body = json!({ "id": self.id, "failed": usage.is_none(), "usage": usage });
        for attempt in 0..3 {
            let result = async {
                expect_ok(
                    post(&self.env, "settle", body.clone()).await?,
                    "settling model usage",
                )
                .await
            }
            .await;
            match result {
                Ok(_) => return,
                Err(cause) if attempt == 2 => {
                    console_error!("settlement {} failed: {cause}", self.id)
                }
                Err(_) => Delay::from(Duration::from_millis(100 * (attempt + 1))).await,
            }
        }
    }
}

async fn denial(mut response: Response) -> Result<Admission> {
    let status = response.status_code();
    let problem = response.json::<Value>().await?;
    let (kind, message) = match status {
        402 => (
            "billing_error",
            "Send /topup in a private chat to buy tokens.",
        ),
        403 => ("permission_error", "This account cannot use the service."),
        429 => (
            "rate_limit_error",
            "Your tokens are reserved for a request in progress.",
        ),
        _ => return Err(error("billing admission is unavailable")),
    };
    Ok(Admission::Refused(refused(
        status,
        kind,
        problem["error"].as_str().unwrap_or(message),
    )?))
}

/// The token-count endpoint accepts the input side of a Messages request.
fn count_body(message: &Value) -> Value {
    let mut count = json!({});
    for key in [
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "output_config",
        "cache_control",
        "billing_policy",
        "compaction",
        "context_management",
        "mcp_servers",
        "speed",
        "user_profile_id",
        "workspace_id",
    ] {
        if let Some(value) = message.get(key) {
            count[key] = value.clone();
        }
    }
    count
}

pub async fn prepare(
    env: &Env,
    user: &str,
    path: &str,
    count_target: &str,
    headers: &Headers,
    body: &mut Vec<u8>,
) -> Result<Admission> {
    if path.trim_end_matches('/').ends_with("/count_tokens") {
        let response = post(env, "access", json!({ "user": user })).await?;
        return if response.status_code() == 200 {
            Ok(Admission::Ready(None))
        } else {
            denial(response).await
        };
    }
    let id = random_uuid();
    let mut hold = json!({ "id": id, "user": user });
    let mut response = post(env, "reserve", hold.clone()).await?;
    if response.status_code() != 200 {
        return denial(response).await;
    }
    let decision: Value = response.json().await?;
    if decision["count_required"] == true {
        let mut message: Value = serde_json::from_slice(body)?;
        let Some(output) = message["max_tokens"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 100_000_000)
        else {
            return Ok(Admission::Refused(refused(
                400,
                "invalid_request_error",
                "A positive max_tokens limit is required.",
            )?));
        };
        let request = rebuilt(
            Method::Post,
            count_target,
            headers.clone(),
            &serde_json::to_vec(&count_body(&message))?,
        )?;
        let mut count = Fetch::Request(request).send().await?;
        if !(200..300).contains(&count.status_code()) {
            let status = count.status_code();
            let headers = kept_headers(&count)?;
            return Ok(Admission::Refused(failure(
                status,
                &count.bytes().await?,
                headers,
            )?));
        }
        let estimate = count.json::<Value>().await?["input_tokens"]
            .as_u64()
            .ok_or_else(|| error("upstream did not count input tokens"))?;
        let minimum = message["thinking"]["budget_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1);
        if minimum > output {
            return Ok(Admission::Refused(refused(
                400,
                "invalid_request_error",
                "The thinking budget must be below max_tokens.",
            )?));
        }
        hold["input"] = estimate.into();
        hold["output"] = output.into();
        hold["min_output"] = minimum.into();
        let mut response = post(env, "reserve", hold).await?;
        if response.status_code() != 200 {
            return denial(response).await;
        }
        let decision = response.json::<Value>().await?;
        if let Some(limit) = decision["max_output"]
            .as_u64()
            .filter(|limit| *limit < output)
        {
            message["max_tokens"] = limit.into();
            *body = serde_json::to_vec(&message)?;
        }
    }
    Ok(Admission::Ready(Some(Reservation {
        env: env.clone(),
        id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_uses_the_same_input_as_generation() {
        let request = json!({"model":"a","messages":[],"tools":[1],"system":"s","cache_control":{"type":"ephemeral"},"max_tokens":100,"stream":true});
        let count = count_body(&request);
        assert_eq!(
            count,
            json!({"model":"a","messages":[],"tools":[1],"system":"s","cache_control":{"type":"ephemeral"}})
        );
    }
}
