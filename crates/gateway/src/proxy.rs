//! The routes agent containers call: `/llm/*` to the LLM gateway, and `/mcp` and `/file`
//! to the shared MCP container. All accept only a valid session token and replace it with
//! the real upstream credential or identity, so no upstream secret ever enters a container.

use agent_proto::{
    Usage,
    token::{self, Claims},
};
use futures_util::StreamExt;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{
    Context, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result, Stub,
    console_error, js_sys::Uint8Array,
};

use crate::{
    config::{Config, LlmAuth},
    mcp_host::{MCP_BINDING, MCP_INSTANCE},
    metering::{SseUsage, json_usage},
    platform::now_ms,
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

/// `ANY /llm/<path>` → `<LLM_BASE_URL>/<path>`. The response, usually an event stream, is
/// passed back as it arrives, and a copy of it is read for the tokens it reports.
pub async fn llm(request: Request, env: &Env, ctx: &Context, cfg: &Config) -> Result<Response> {
    let Some(claims) = authenticate(&request, cfg)? else {
        return unauthorized();
    };
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

    // Metering must never cost the agent its answer: anything that goes wrong with the
    // copy is logged and the original is returned regardless.
    if (200..300).contains(&response.status_code()) {
        let session = env
            .durable_object(SESSION_BINDING)
            .and_then(|namespace| namespace.get_by_name(&claims.session));
        match (response.cloned(), session) {
            (Ok(copy), Ok(session)) => ctx.wait_until(async move {
                if let Err(error) = meter(copy, session).await {
                    console_error!("metering a model call failed: {error}");
                }
            }),
            (Err(error), _) | (_, Err(error)) => {
                console_error!("a model call could not be metered: {error}");
            }
        }
    }
    Ok(response)
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
