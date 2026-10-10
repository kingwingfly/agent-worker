//! The control surface the Durable Object talks to.
//!
//! The agent runs in the same container and could reach these endpoints over loopback,
//! which would let it approve its own permission prompts. So every endpoint except the
//! health check needs a secret the Durable Object sets through `POST /claim` right after
//! the container starts, before any agent process exists. The secret lives only in this
//! process's memory, which [`crate::harden_process`] makes unreadable to the agent.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use agent_proto::{
    FromRunner, ToRunner,
    token::{self, constant_time_eq},
};
use axum::{
    Router,
    body::Body,
    extract::{
        Path, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::{config::Config, session::SessionHandle, state};

const MIN_CONTROL_SECRET_LEN: usize = 32;
const HEARTBEAT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct App {
    pub cfg: Arc<Config>,
    pub session: SessionHandle,
    control_secret: Arc<OnceLock<String>>,
}

type Failure = (StatusCode, String);

fn internal(error: impl std::fmt::Display) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
}

pub fn router(cfg: Arc<Config>, session: SessionHandle) -> Router {
    let app = App {
        cfg,
        session,
        control_secret: Arc::default(),
    };
    let control = Router::new()
        .route("/ws", get(websocket))
        .route("/bootstrap", post(bootstrap))
        .route("/state", get(export_state))
        .route("/files/out/{*path}", get(artifact))
        .layer(middleware::from_fn_with_state(app.clone(), require_control));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/claim", post(claim))
        .merge(control)
        .with_state(app)
}

/// Sets the control secret. Succeeds once, with 201; repeating the same secret is a
/// no-op answered with 204, so the Durable Object can retry and can tell a runner it is
/// claiming for the first time, which has none of the session's files yet, from one it
/// already holds.
async fn claim(State(app): State<App>, body: String) -> StatusCode {
    let secret = body.trim();
    if secret.len() < MIN_CONTROL_SECRET_LEN {
        return StatusCode::BAD_REQUEST;
    }
    match app.control_secret.set(secret.to_owned()) {
        Ok(()) => StatusCode::CREATED,
        Err(_) if constant_time_eq(app.control_secret.get().expect("just seen set"), secret) => {
            StatusCode::NO_CONTENT
        }
        Err(_) => StatusCode::CONFLICT,
    }
}

async fn require_control(State(app): State<App>, request: Request, next: Next) -> Response {
    let Some(expected) = app.control_secret.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "runner has not been claimed",
        )
            .into_response();
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(token::from_header);
    match presented {
        Some(presented) if constant_time_eq(presented, expected) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn websocket(State(app): State<App>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| serve_socket(socket, app.session))
}

async fn serve_socket(mut socket: WebSocket, session: SessionHandle) {
    let mut outgoing = session.attach();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    loop {
        let frame = tokio::select! {
            frame = outgoing.recv() => match frame {
                Some(frame) => frame,
                // A newer connection took over.
                None => break,
            },
            _ = heartbeat.tick() => FromRunner::Heartbeat,
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ToRunner>(&text) {
                        Ok(frame) => session.handle(frame),
                        Err(error) => tracing::warn!(%error, "ignoring unreadable frame"),
                    }
                    continue;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };
        let text = serde_json::to_string(&frame).expect("frames serialize");
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

/// Restores a state archive. Only possible before Claude has started, since Claude reads
/// its transcript at launch.
async fn bootstrap(State(app): State<App>, request: Request) -> Result<StatusCode, Failure> {
    if app.session.started().await {
        return Err((StatusCode::CONFLICT, "claude has already started".into()));
    }
    let archive = NamedTempFile::new_in(&app.cfg.runtime_dir).map_err(internal)?;
    let mut file = tokio::fs::File::from_std(archive.reopen().map_err(internal)?);
    let mut body = request.into_body().into_data_stream();
    let mut received = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        received += chunk.len() as u64;
        if received > app.cfg.state_max_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "state archive too large".into(),
            ));
        }
        file.write_all(&chunk).await.map_err(internal)?;
    }
    file.flush().await.map_err(internal)?;

    let cfg = app.cfg.clone();
    tokio::task::spawn_blocking(move || state::import(&cfg, archive.path()))
        .await
        .map_err(internal)?
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

fn file_response(file: std::fs::File, content_type: &'static str) -> Result<Response, Failure> {
    let length = file.metadata().map_err(internal)?.len();
    let stream = ReaderStream::new(tokio::fs::File::from_std(file));
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(stream))
        .map_err(internal)
}

async fn export_state(State(app): State<App>) -> Result<Response, Failure> {
    let cfg = app.cfg.clone();
    let archive = tokio::task::spawn_blocking(move || state::export(&cfg))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    // The reopened handle keeps the data readable after the temporary file is unlinked.
    file_response(archive.reopen().map_err(internal)?, "application/gzip")
}

async fn artifact(State(app): State<App>, Path(path): Path<String>) -> Result<Response, Failure> {
    let not_found = || (StatusCode::NOT_FOUND, "no such artifact".to_owned());
    let file = state::resolve_artifact(&app.cfg.out_dir, &path).ok_or_else(not_found)?;
    let file = std::fs::File::open(file).map_err(|_| not_found())?;
    file_response(file, "application/octet-stream")
}
