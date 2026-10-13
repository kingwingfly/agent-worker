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
    session::{GONE, NAME_HEADER, SESSION_BINDING, UPDATE_PATH},
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

async fn telegram(request: Request, env: &Env, cfg: &Config) -> Result<Response> {
    let presented = request
        .headers()
        .get("x-telegram-bot-api-secret-token")?
        .unwrap_or_default();
    if !constant_time_eq(&presented, &cfg.webhook_secret) {
        return Response::error("forbidden", 403);
    }
    // From here on Telegram always gets a 200: anything else makes it send the update
    // again. So nothing that goes wrong with the update is this request's error.
    if let Err(error) = take_update(request, env, cfg).await {
        console_error!("an update could not be handled: {error}");
    }
    Response::empty()
}

/// Hands an update to the session it is for, if it is for one and its user is let in.
async fn take_update(mut request: Request, env: &Env, cfg: &Config) -> Result<()> {
    let body = request.text().await?;
    let Ok(update) = serde_json::from_str::<Update>(&body) else {
        return Ok(());
    };
    let Some(origin) = update.origin() else {
        // A button nothing is known about still shows as pressed until it is answered.
        if let Some(query) = &update.callback_query {
            let _ = Bot::new(cfg).answer_callback(&query.id, Some(GONE)).await;
        }
        return Ok(());
    };
    if !cfg.allowed_users.admits(origin.user_id) {
        turn_away(cfg, &update, origin.user_id).await;
        return Ok(());
    }

    let name = origin.session;
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
    Ok(())
}

/// Tells a user the allow-list leaves out that the bot is not theirs to use, in the way
/// the update they caused can be answered: with a message, with the one reply a guest
/// summon gets, or over the button they pressed. The id is what the operator needs to
/// let them in.
async fn turn_away(cfg: &Config, update: &Update, user_id: i64) {
    let bot = Bot::new(cfg);
    let private = "This bot is private. Your Telegram user id is";
    let notice = format!("{private} <code>{user_id}</code>.");
    let told = if let Some(message) = &update.message {
        bot.send_message(message.chat.id, &notice, None)
            .await
            .map(drop)
    } else if let Some(query_id) = update
        .guest_message
        .as_ref()
        .and_then(|message| message.guest_query_id.as_deref())
    {
        bot.answer_guest(query_id, &notice, None).await.map(drop)
    } else if let Some(query) = &update.callback_query {
        let notice = format!("{private} {user_id}.");
        bot.answer_callback(&query.id, Some(&notice)).await
    } else {
        // Nobody is waiting for an answer to a press on a draft's Stop button.
        Ok(())
    };
    if let Err(error) = told {
        console_error!("a user who is not let in could not be told: {error}");
    }
}
