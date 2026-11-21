//! An optional service binding. Prices, accounts and payment credentials live in the billing Worker.
pub mod model;
mod telegram;

pub use telegram::{intercept, payment_update};

use agent_proto::TurnResult;
use serde_json::{Value, json};
use wasm_bindgen::JsValue;
use worker::{Env, Headers, Method, RequestInit, Response, Result};

use crate::platform::{error, expect_ok};

pub const PAYER_HEADER: &str = "x-billing-payer";
pub const HELP: &str = "\n/apply — request access in private\n/account — access status, usage and private account sign-in\n/unsubscribe — stop monthly renewal\n/paysupport — account and payment support\nOperators can review saved chats and output files.";

async fn post(env: &Env, action: &str, body: Value) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body.to_string())));
    env.service("BILLING")?
        .fetch(format!("https://billing/{action}"), Some(init))
        .await
}

pub async fn member_allowed(env: &Env, user: &str) -> Result<bool> {
    match post(env, "welcome", json!({ "user": user }))
        .await?
        .status_code()
    {
        200 => Ok(true),
        403 => Ok(false),
        _ => Err(error("chat membership is unavailable")),
    }
}

pub async fn record(
    env: &Env,
    user: i64,
    id: &str,
    session: &str,
    prompt: &str,
    started: u64,
    result: &TurnResult,
) -> Result<()> {
    let body = json!({
        "user": user.to_string(), "id": id, "session": session, "created_at": started,
        "prompt": prompt.chars().take(64_000).collect::<String>(),
        "answer": result.text.as_deref().unwrap_or_default().chars().take(64_000).collect::<String>(),
        "is_error": result.is_error,
    });
    expect_ok(
        post(env, "record", body).await?,
        "recording billing history",
    )
    .await
    .map(drop)
}
