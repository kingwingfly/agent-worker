//! The routes agent containers call: `/llm/*` to the LLM gateway, and `/mcp` and `/file`
//! to the shared MCP container. All accept only a valid session token and replace it with
//! the real upstream credential or identity, so no upstream secret ever enters a container.
//! Nor does anything that says which gateway it is: see [`scrub`].

use agent_proto::{
    Usage,
    token::{self, Claims},
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{
    ByteStream, Context, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result, Stub,
    console_error, js_sys::Uint8Array,
};

use crate::{
    config::{Config, LlmAuth},
    mcp_host::{MCP_BINDING, MCP_INSTANCE},
    metering::{SseUsage, json_usage},
    platform::now_ms,
    scrub::{self, SseScrub},
    session::{SESSION_BINDING, USAGE_PATH},
};

/// Header telling the MCP server which session a call belongs to. Set here from the
/// verified token; a value sent by the container is discarded.
const SESSION_HEADER: &str = "x-agent-session";

fn authenticate(request: &Request, cfg: &Config) -> Result<Option<Claims>> {
    let headers = request.headers();
    let presented = match headers.get("authorization")? {
        Some(value) => Some(value),
        None => headers.get("x-api-key")?,
    };
    Ok(presented.and_then(|value| {
        let token = token::from_header(&value);
        token::verify(cfg.token_secret.as_bytes(), token, now_ms() / 1000).ok()
    }))
}

/// A 401 in the shape of an Anthropic API error, which Claude Code reports cleanly.
fn unauthorized() -> Result<Response> {
    let body = json!({
        "type": "error",
        "error": { "type": "authentication_error", "message": "invalid or expired session token" },
    });
    Ok(Response::from_json(&body)?.with_status(401))
}

/// Copies the request headers minus credentials and everything describing the hop from
/// the container to this Worker.
fn forwarded_headers(request: &Request) -> Result<Headers> {
    const DROPPED: [&str; 8] = [
        "authorization",
        "x-api-key",
        "host",
        "content-length",
        "connection",
        "cdn-loop",
        "x-real-ip",
        SESSION_HEADER,
    ];
    let headers = Headers::new();
    for (name, value) in request.headers().entries() {
        let name = name.to_ascii_lowercase();
        let hop = name.starts_with("cf-") || name.starts_with("x-forwarded-");
        if !hop && !DROPPED.contains(&name.as_str()) {
            headers.append(&name, &value)?;
        }
    }
    Ok(headers)
}

/// Rebuilds the request for `target`. The body is buffered: these are JSON requests, and
/// a buffered body can be sent anywhere without the caveats of a streamed one.
async fn retarget(mut request: Request, target: &str, headers: Headers) -> Result<Request> {
    let body = request.bytes().await?;
    let mut init = RequestInit::new();
    init.with_method(request.method()).with_headers(headers);
    if !body.is_empty() {
        init.with_body(Some(JsValue::from(Uint8Array::from(body.as_slice()))));
    }
    Request::new_with_init(target, &init)
}

/// How much of what the gateway said goes into one line of the log.
const LOGGED_CHARS: usize = 2000;

fn excerpt(said: &[u8]) -> String {
    String::from_utf8_lossy(said)
        .chars()
        .take(LOGGED_CHARS)
        .collect()
}

/// The headers of the gateway's response that are passed on.
fn kept_headers(response: &Response) -> Result<Headers> {
    let headers = Headers::new();
    for name in scrub::KEPT_HEADERS {
        if let Some(value) = response.headers().get(name)? {
            headers.set(name, &value)?;
        }
    }
    Ok(headers)
}

/// The answer to a model call that failed, in place of the gateway's own: what kind of
/// failure it was, and nothing about where it happened.
fn failure(status: u16, said: &[u8], headers: Headers) -> Result<Response> {
    headers.set("content-type", "application/json")?;
    let body = scrub::error_body(Some(status), said);
    Ok(Response::from_json(&body)?
        .with_status(status)
        .with_headers(headers))
}

/// An event stream on its way to the container, with the errors in it replaced. What
/// they said is logged for `session`.
fn scrubbed(body: ByteStream, session: String) -> impl Stream<Item = Result<Vec<u8>>> {
    stream::unfold(Some((body, SseScrub::default())), move |state| {
        let session = session.clone();
        async move {
            let (mut body, mut scrub) = state?;
            loop {
                let next = body.next().await;
                let ended = next.is_none();
                let out = match next {
                    Some(Ok(chunk)) => scrub.feed(&chunk),
                    Some(Err(error)) => return Some((Err(error), None)),
                    None => scrub.finish(),
                };
                for said in scrub.replaced.drain(..) {
                    let said = excerpt(said.as_bytes());
                    console_error!("{session}: the gateway failed inside its answer: {said}");
                }
                // Nothing to send yet means a line that has not arrived in full.
                match (out.is_empty(), ended) {
                    (false, false) => return Some((Ok(out), Some((body, scrub)))),
                    (false, true) => return Some((Ok(out), None)),
                    (true, true) => return None,
                    (true, false) => {}
                }
            }
        }
    })
}

/// `ANY /llm/<path>` → `<LLM_BASE_URL>/<path>`. The response, usually an event stream, is
/// passed back as it arrives, and a copy of it is read for the tokens it reports.
///
/// An error is not passed back as it arrives, whichever way the gateway reports it: with
/// a status, inside the stream, or as a success that is none. The container gets one of
/// our own making and the log gets the gateway's.
pub async fn llm(request: Request, env: &Env, ctx: &Context, cfg: &Config) -> Result<Response> {
    let Some(claims) = authenticate(&request, cfg)? else {
        return unauthorized();
    };
    let session = &claims.session;
    forward(request, env, ctx, cfg, session)
        .await
        .or_else(|error| {
            // Not returned as it is: the runtime's own errors can quote the address that
            // was being called.
            console_error!("{session}: a model call failed in the Worker: {error}");
            failure(502, b"", Headers::new())
        })
}

async fn forward(
    request: Request,
    env: &Env,
    ctx: &Context,
    cfg: &Config,
    session: &str,
) -> Result<Response> {
    let url = request.url()?;
    let path = url.path().strip_prefix("/llm").unwrap_or_default();
    let query = url
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let target = format!("{}{path}{query}", cfg.llm_base_url);

    let headers = forwarded_headers(&request)?;
    match cfg.llm_auth {
        LlmAuth::Bearer => headers.set("authorization", &format!("Bearer {}", cfg.llm_api_key))?,
        LlmAuth::ApiKey => headers.set("x-api-key", &cfg.llm_api_key)?,
    }
    let mut response = Fetch::Request(retarget(request, &target, headers).await?)
        .send()
        .await?;
    let status = response.status_code();
    let headers = kept_headers(&response)?;
    if !(200..300).contains(&status) {
        let said = response.bytes().await.unwrap_or_default();
        let logged = excerpt(&said);
        console_error!("{session}: the gateway answered {status}: {logged}");
        return failure(status, &said, headers);
    }

    // Metering must never cost the agent its answer: anything that goes wrong with the
    // copy is logged and the original is returned regardless.
    let stub = env
        .durable_object(SESSION_BINDING)
        .and_then(|namespace| namespace.get_by_name(session));
    match (response.cloned(), stub) {
        (Ok(copy), Ok(stub)) => ctx.wait_until(async move {
            if let Err(error) = meter(copy, stub).await {
                console_error!("metering a model call failed: {error}");
            }
        }),
        (Err(error), _) | (_, Err(error)) => {
            console_error!("a model call could not be metered: {error}");
        }
    }

    let streamed = headers
        .get("content-type")?
        .is_some_and(|kind| kind.starts_with("text/event-stream"));
    if streamed {
        let body = scrubbed(response.stream()?, session.to_owned());
        return Ok(Response::from_stream(body)?
            .with_status(status)
            .with_headers(headers));
    }
    let body = response.bytes().await?;
    if !scrub::is_answer(&body) {
        let logged = excerpt(&body);
        console_error!("{session}: the gateway answered {status} with no answer: {logged}");
        return failure(502, &body, headers);
    }
    Ok(Response::from_bytes(body)?
        .with_status(status)
        .with_headers(headers))
}

/// Reads a copy of a model response to its end and reports the tokens to the session.
async fn meter(mut copy: Response, session: Stub) -> Result<()> {
    let streamed = copy
        .headers()
        .get("content-type")?
        .is_some_and(|kind| kind.starts_with("text/event-stream"));
    let usage: Usage = if streamed {
        let mut scanner = SseUsage::default();
        let mut body = copy.stream()?;
        // A stream that breaks off still counts for what it had reported by then.
        while let Some(Ok(chunk)) = body.next().await {
            scanner.feed(&chunk);
        }
        scanner.usage()
    } else {
        json_usage(&copy.bytes().await?)
    };
    let tokens = usage.billable();
    if tokens == 0 {
        return Ok(());
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from_str(
            &json!({ "tokens": tokens }).to_string(),
        )));
    let report = Request::new_with_init(&format!("https://session{USAGE_PATH}"), &init)?;
    session.fetch_with_request(report).await.map(drop)
}

/// `ANY /mcp[/...]` and `/file` → the shared MCP container, by way of its Durable Object.
/// The path is kept, and the container's own proxy tells the two apart.
pub async fn mcp(request: Request, env: &Env, cfg: &Config) -> Result<Response> {
    let Some(claims) = authenticate(&request, cfg)? else {
        return unauthorized();
    };
    let url = request.url()?;
    let query = url
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let base = match &cfg.dev_mcp_url {
        Some(url) => url.clone(),
        // `localhost` is what an rmcp server accepts as `Host` out of the box.
        None => format!("http://localhost:{}", agent_proto::MCP_PORT),
    };
    let target = format!("{base}{}{query}", url.path());

    let headers = forwarded_headers(&request)?;
    headers.set(SESSION_HEADER, &claims.session)?;
    let forwarded = retarget(request, &target, headers).await?;
    if cfg.dev_mcp_url.is_some() {
        return Fetch::Request(forwarded).send().await;
    }
    env.durable_object(MCP_BINDING)?
        .get_by_name(MCP_INSTANCE)?
        .fetch_with_request(forwarded)
        .await
}
