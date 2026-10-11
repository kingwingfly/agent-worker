//! The Cloudflare Worker in front of everything.
//!
//! - `POST /tg/webhook` takes Telegram updates and hands each to its chat's `SessionDO`.
//! - `/llm/*`, `/mcp` and `/file` are what agent containers call; see [`proxy`].
//!
//! Everything is built for the host as well as for wasm32, so editors and `cargo test`
//! see the whole crate. Only `calls`, `metering`, `remember`, `render`, `scrub` and
//! `update` can actually run there: the rest calls into the Workers runtime, which
//! exists only in a Worker.

pub mod calls;
pub mod metering;
pub mod remember;
pub mod render;
pub mod scrub;
pub mod update;

mod bot;
mod config;
mod container_ext;
mod mcp_host;
mod platform;
mod proxy;
mod session;
mod store;

use agent_proto::token::constant_time_eq;
use wasm_bindgen::JsValue;
use worker::{
    Context, Env, Headers, Method, Request, RequestInit, Response, Result, console_error, event,
};

use crate::{
    bot::Bot,
    config::Config,
    session::{NAME_HEADER, SESSION_BINDING, UPDATE_PATH},
    update::Update,
};

#[event(fetch)]
async fn fetch(request: Request, env: Env, ctx: Context) -> Result<Response> {
    let path = request.path();
    if path == "/healthz" {
        return Response::ok("ok");
    }
    let cfg = match Config::load(&env) {
        Ok(cfg) => cfg,
        Err(error) => {
            console_error!("the service is misconfigured: {error}");
            return Response::error("the service is not configured", 500);
        }
    };
    match path.as_str() {
        "/tg/webhook" if request.method() == Method::Post => telegram(request, &env, &cfg).await,
        path if path.starts_with("/llm/") => proxy::llm(request, &env, &ctx, &cfg).await,
        // Both are answered inside the MCP container: MCP calls by the MCP server, and
        // requests for the files it saved by the file server next to it.
        path if under(path, "/mcp") || under(path, "/file") => {
            proxy::mcp(request, &env, &cfg).await
        }
        _ => Response::error("not found", 404),
    }
}

/// Whether `path` is `prefix` itself or something below it.
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

async fn telegram(mut request: Request, env: &Env, cfg: &Config) -> Result<Response> {
    let presented = request
        .headers()
        .get("x-telegram-bot-api-secret-token")?
        .unwrap_or_default();
    if !constant_time_eq(&presented, &cfg.webhook_secret) {
        return Response::error("forbidden", 403);
    }
    // From here on Telegram always gets a 200: anything else makes it send the update
    // again.
    let body = request.text().await?;
    let Ok(update) = serde_json::from_str::<Update>(&body) else {
        return Response::empty();
    };
    let Some((chat_id, user_id)) = update.origin() else {
        return Response::empty();
    };
    if !cfg.allowed_users.admits(user_id) {
        let notice =
            format!("This bot is private. Your Telegram user id is <code>{user_id}</code>.");
        let _ = Bot::new(cfg).send_message(chat_id, &notice, None).await;
        return Response::empty();
    }

    let name = format!("tg:{chat_id}");
    let headers = Headers::new();
    headers.set(NAME_HEADER, &name)?;
    headers.set("content-type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body)));
    let forwarded = Request::new_with_init(&format!("https://session{UPDATE_PATH}"), &init)?;
    let stub = env.durable_object(SESSION_BINDING)?.get_by_name(&name)?;
    if let Err(error) = stub.fetch_with_request(forwarded).await {
        console_error!("session {name} failed: {error}");
    }
    Response::empty()
}
