//! One chat's session: a Durable Object that owns the agent container, the conversation
//! state and everything shown in Telegram.
//!
//! All durable state is in [`Session`] and the approvals table. Work is pushed forward by
//! [`Inner::drive`], which looks at that state and does whatever is missing (start the
//! container, restore its files, connect, hand over the message). It is safe to call at
//! any time, and an alarm calls it periodically while a turn is open, so a Durable Object
//! that was evicted or a container that died mid-turn is picked up again.

use std::{cell::RefCell, rc::Rc, time::Duration};

use agent_proto::{
    ASK_USER_QUESTION, Artifact, Decision, FromRunner, RUNNER_PORT, ToRunner, TurnResult,
    env as runner_env,
    token::{self, Claims},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasm_bindgen::{JsValue, UnwrapThrowExt};
use worker::{
    Container, Delay, DurableObject, Env, Fetch, FixedLengthStream, Headers, Method, Request,
    RequestInit, Response, ResponseBody, Result, State, Url, WebSocket, WebsocketEvent,
    console_error, console_warn, durable_object, js_sys::Uint8Array, wasm_bindgen_futures,
};

use crate::{
    bot::Bot,
    config::Config,
    container_ext,
    mcp_host::{MCP_BINDING, MCP_INSTANCE, WARM_PATH},
    platform::{error, expect_ok, now_ms, random_uuid},
    remember,
    render::{self, Callback, Status},
    store::{Approval, Store},
    update::{CallbackQuery, Update},
};

pub const SESSION_BINDING: &str = "SESSION";
/// Carries the Durable Object's name, which it cannot always read from its own id.
pub const NAME_HEADER: &str = "x-session-name";
/// Requests to the object: a Telegram update, or tokens spent as seen by the LLM proxy.
pub const UPDATE_PATH: &str = "/update";
pub const USAGE_PATH: &str = "/usage";

const STATE_BUCKET: &str = "STATE";
const SESSION_KEY: &str = "session";

const WATCHDOG_MS: u64 = 30_000;
const RETRY_MS: u64 = 2_000;
const STATUS_EDIT_INTERVAL_MS: u64 = 2_000;
/// Stream events between acknowledgments. Prompts and results are acknowledged at once.
const ACK_EVERY: u32 = 64;
const MAX_DRIVE_FAILURES: u32 = 4;
const READY_POLL_MS: u64 = 300;
const READY_ATTEMPTS: u32 = 100;
const TOKEN_TTL_SECS: u64 = 24 * 60 * 60;
/// A container whose token has less than this left is replaced before a new turn.
const TOKEN_MARGIN_SECS: u64 = 60 * 60;
/// How far past the turn timeout a turn may run before it is given up on.
const STOP_GRACE_MS: u64 = 120_000;
const KEEP_ALIVE_MARGIN_MS: u64 = 60_000;
/// Larger files are kept in R2 but not uploaded to the chat: the upload is assembled in
/// memory, and a Worker has little of it.
const TELEGRAM_FILE_LIMIT: u64 = 10 * 1024 * 1024;
/// Pauses before each attempt at sending a message that must arrive.
const DELIVERY_BACKOFF_MS: [u64; 3] = [0, 2_000, 8_000];
const MS_PER_DAY: u64 = 86_400_000;

const HELP: &str = "Send me a message and I'll get to work.\n\n\
    /new — start a fresh conversation\n\
    /stop — stop what I'm doing\n\
    /status — what I'm doing and today's usage";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Session {
    /// Durable Object name, e.g. `tg:12345`.
    name: String,
    chat_id: i64,
    claude_session_id: String,
    /// Secret the current container's runner was claimed with.
    control_secret: String,
    /// Unix seconds at which the current container's session token expires.
    token_expires_at: u64,
    /// A state archive for this session exists in R2.
    has_state: bool,
    /// The current container was started with an empty disk and has not been given the
    /// archive yet. Until it has, it must neither run a turn nor be saved from: saving
    /// would replace the archive with nothing.
    #[serde(default)]
    needs_restore: bool,
    /// Last runner frame that has been fully handled.
    last_seq: u64,
    turn: Option<Turn>,
    /// When a warm, idle container is stopped.
    idle_deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Turn {
    id: String,
    text: String,
    started_at_ms: u64,
    /// The message that shows the turn's progress. It is edited as the turn goes on and
    /// stays in the chat when it is over.
    status_message_id: Option<i64>,
    /// The message has been handed to the runner, so stopping means interrupting it.
    #[serde(default)]
    delivered: bool,
    #[serde(default)]
    stop_requested: bool,
    /// Consecutive failed attempts to get the turn to the runner.
    #[serde(default)]
    failures: u32,
}

#[derive(Deserialize)]
struct UsageReport {
    tokens: u64,
}

/// What the runner said to being claimed.
enum Claim {
    /// It had never been claimed: a new process, with none of the session's files.
    Fresh,
    /// It was already claimed with this secret.
    Held,
    /// It is claimed with a secret this object does not have.
    Lost,
}

/// Where the session's runner lives.
enum Host {
    /// The Durable Object's own container.
    Container(Container),
    /// A runner started by hand (`DEV_RUNNER_URL`), for working on this code without a
    /// container engine. It is assumed to be up and is never started or stopped.
    External(String),
}

impl Host {
    fn running(&self) -> bool {
        match self {
            Self::Container(container) => container.running(),
            Self::External(_) => true,
        }
    }

    async fn fetch(&self, path: &str, init: RequestInit) -> Result<Response> {
        match self {
            Self::Container(container) => {
                let port = container.get_tcp_port(RUNNER_PORT)?;
                port.fetch(format!("http://runner{path}"), Some(init)).await
            }
            Self::External(base) => {
                let request = Request::new_with_init(&format!("{base}{path}"), &init)?;
                Fetch::Request(request).send().await
            }
        }
    }

    async fn stop(&self) -> Result<()> {
        match self {
            Self::Container(container) if container.running() => container.destroy(None).await,
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
struct Live {
    socket: Option<WebSocket>,
    driving: bool,
    status: Status,
    last_status_edit_ms: u64,
    unacked_events: u32,
}

struct Inner {
    state: State,
    env: Env,
    store: Store,
    session: RefCell<Session>,
    live: RefCell<Live>,
}

#[durable_object]
pub struct SessionDO {
    inner: Rc<Inner>,
}

impl DurableObject for SessionDO {
    fn new(state: State, env: Env) -> Self {
        let store = Store::open(state.storage().sql()).expect_throw("opening session storage");
        let session = store.load(SESSION_KEY).ok().flatten().unwrap_or_default();
        Self {
            inner: Rc::new(Inner {
                state,
                env,
                store,
                session: RefCell::new(session),
                live: RefCell::default(),
            }),
        }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        if request.path() == USAGE_PATH {
            let report: UsageReport = request.json().await?;
            self.inner.store.add_usage(today(), report.tokens)?;
            return Response::empty();
        }
        let name = request.headers().get(NAME_HEADER)?.unwrap_or_default();
        let update: Update = request.json().await?;
        if let Err(error) = self.inner.on_update(&name, update).await {
            console_error!("handling an update failed: {error}");
        }
        Response::empty()
    }

    async fn alarm(&self) -> Result<Response> {
        if let Err(error) = self.inner.on_alarm().await {
            console_error!("alarm failed: {error}");
            // The alarm is what notices a stuck turn and stops an idle container. If it
            // gave up here, nothing would ever look again.
            self.inner.arm(WATCHDOG_MS).await;
        }
        Response::empty()
    }
}

fn today() -> u64 {
    now_ms() / MS_PER_DAY
}

impl Inner {
    fn cfg(&self) -> Result<Config> {
        Config::load(&self.env)
    }

    fn host(&self) -> Result<Host> {
        if let Some(url) = self.cfg()?.dev_runner_url {
            return Ok(Host::External(url));
        }
        self.state
            .container()
            .map(Host::Container)
            .ok_or_else(|| error("SessionDO has no container"))
    }

    /// Changes the session and writes it back.
    fn update<T>(&self, change: impl FnOnce(&mut Session) -> T) -> T {
        let mut session = self.session.borrow_mut();
        let out = change(&mut session);
        if let Err(error) = self.store.save(SESSION_KEY, &*session) {
            console_error!("saving the session failed: {error}");
        }
        out
    }

    fn turn(&self) -> Option<Turn> {
        self.session.borrow().turn.clone()
    }

    fn chat_id(&self) -> i64 {
        self.session.borrow().chat_id
    }

    fn state_key(&self) -> String {
        format!("sessions/{}/state.tar.gz", self.session.borrow().name)
    }

    async fn arm(&self, delay_ms: u64) {
        let storage = self.state.storage();
        if let Err(error) = storage.set_alarm(Duration::from_millis(delay_ms)).await {
            console_error!("setting the alarm failed: {error}");
        }
    }

    /// Sends a frame to the runner. Returns whether it went out; a socket that cannot
    /// send is dropped, so the next `drive` connects again.
    fn send(&self, frame: &ToRunner) -> bool {
        let text = serde_json::to_string(frame).expect_throw("frames serialize");
        let mut live = self.live.borrow_mut();
        let sent = live
            .socket
            .as_ref()
            .is_some_and(|socket| socket.send_with_str(&text).is_ok());
        if !sent {
            live.socket = None;
        }
        sent
    }

    /// Calls the runner's control API.
    async fn runner(&self, method: Method, path: &str, body: Option<JsValue>) -> Result<Response> {
        let headers = Headers::new();
        let secret = self.session.borrow().control_secret.clone();
        headers.set("authorization", &format!("Bearer {secret}"))?;
        let mut init = RequestInit::new();
        init.with_method(method)
            .with_headers(headers)
            .with_body(body);
        self.host()?.fetch(path, init).await
    }

    /// Sends a message that must not be lost, trying again after a pause when it fails.
    async fn deliver(&self, bot: &Bot, html: &str) -> Result<()> {
        let mut outcome = Ok(());
        for pause in DELIVERY_BACKOFF_MS {
            if pause > 0 {
                Delay::from(Duration::from_millis(pause)).await;
            }
            outcome = bot.send_message(self.chat_id(), html, None).await.map(drop);
            if outcome.is_ok() {
                break;
            }
        }
        outcome
    }

    // ----- Telegram updates -----

    async fn on_update(self: &Rc<Self>, name: &str, update: Update) -> Result<()> {
        let cfg = self.cfg()?;
        let bot = Bot::new(&cfg);
        if let Some(message) = update.message {
            if self.session.borrow().name.is_empty() {
                self.update(|session| {
                    session.name = name.to_owned();
                    session.chat_id = message.chat.id;
                    session.claude_session_id = random_uuid();
                });
            }
            match message.text.as_deref().map(str::trim) {
                Some(text) if !text.is_empty() => self.on_text(&cfg, &bot, text).await,
                _ => bot
                    .send_message(message.chat.id, "I can only read text messages.", None)
                    .await
                    .map(drop),
            }
        } else if let Some(query) = update.callback_query {
            self.on_callback(&cfg, &bot, query).await
        } else {
            Ok(())
        }
    }

    async fn on_text(self: &Rc<Self>, cfg: &Config, bot: &Bot, text: &str) -> Result<()> {
        let chat_id = self.chat_id();
        // Commands may arrive as `/stop@name_of_the_bot`.
        let command = text
            .split_whitespace()
            .next()
            .filter(|word| word.starts_with('/'))
            .map(|word| word.split('@').next().unwrap_or(word));
        let reply = match command {
            None => return self.begin_turn(cfg, bot, text).await,
            Some("/start" | "/help") => HELP.to_owned(),
            Some("/new") => self.reset().await?,
            Some("/stop") => self.stop(cfg, bot).await,
            Some("/status") => self.describe(cfg)?,
            Some(other) => format!("I don't know {}.\n\n{HELP}", render::escape(other)),
        };
        bot.send_message(chat_id, &reply, None).await.map(drop)
    }

    async fn begin_turn(self: &Rc<Self>, cfg: &Config, bot: &Bot, text: &str) -> Result<()> {
        let chat_id = self.chat_id();
        let refusal = if self.turn().is_some() {
            Some("I'm still working on your previous message. Send /stop to cancel it.")
        } else if cfg.daily_token_budget > 0 && self.store.usage(today())? >= cfg.daily_token_budget
        {
            Some("Today's usage limit for this chat is used up. Try again tomorrow.")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            return bot.send_message(chat_id, refusal, None).await.map(drop);
        }

        // Record the turn before the first await, so a second message cannot slip in.
        self.update(|session| {
            session.turn = Some(Turn {
                id: random_uuid(),
                text: text.to_owned(),
                started_at_ms: now_ms(),
                status_message_id: None,
                delivered: false,
                stop_requested: false,
                failures: 0,
            });
        });
        {
            let mut live = self.live.borrow_mut();
            live.status = Status::default();
            live.last_status_edit_ms = 0;
        }
        let status = bot.send_message(chat_id, render::WORKING, None).await.ok();
        self.update(|session| {
            if let Some(turn) = &mut session.turn {
                turn.status_message_id = status;
            }
        });

        // From here on a failure must not leave the turn open with nothing working on it.
        if let Err(error) = self.launch().await {
            let message = format!(
                "⚠️ I could not start: {}",
                render::escape(&error.to_string())
            );
            self.abandon_turn(&message).await;
        }
        Ok(())
    }

    async fn launch(self: &Rc<Self>) -> Result<()> {
        // A container about to outlive its token is replaced while nothing is running.
        let host = self.host()?;
        let expires_at = self.session.borrow().token_expires_at;
        if matches!(host, Host::Container(_))
            && host.running()
            && expires_at < now_ms() / 1000 + TOKEN_MARGIN_SECS
        {
            self.live.borrow_mut().socket = None;
            host.stop().await?;
        }

        self.warm_mcp();
        self.arm(WATCHDOG_MS).await;
        let this = self.clone();
        self.state.wait_until(async move { this.drive().await });
        Ok(())
    }

    /// `/new`: forget the conversation and its files.
    async fn reset(&self) -> Result<String> {
        if self.turn().is_some() {
            return Ok("Stop the current task with /stop first.".into());
        }
        self.live.borrow_mut().socket = None;
        if let Ok(host) = self.host() {
            host.stop().await?;
        }
        self.env
            .bucket(STATE_BUCKET)?
            .delete(self.state_key())
            .await?;
        self.store.clear_approvals()?;
        self.store.forget_all()?;
        self.update(|session| {
            session.claude_session_id = random_uuid();
            session.has_state = false;
            session.needs_restore = false;
            session.last_seq = 0;
        });
        Ok("Started a fresh conversation.".into())
    }

    /// `/stop`.
    async fn stop(&self, cfg: &Config, bot: &Bot) -> String {
        let Some(turn) = self.turn() else {
            return "Nothing is running.".into();
        };
        if !turn.delivered {
            // The runner has not been given the message, so there is nothing to
            // interrupt: the turn simply ends here. A `drive` on its way to the runner
            // reads the turn again before sending and finds it gone.
            self.close_turn(cfg.idle_ttl_ms).await;
            self.close_status(bot, &turn, render::STOPPED).await;
            return "⏹ Stopped.".into();
        }
        self.update(|session| {
            if let Some(turn) = &mut session.turn {
                turn.stop_requested = true;
            }
        });
        if !self.send(&ToRunner::Interrupt) {
            // Not connected right now; `drive` reconnects and passes the interrupt on.
            self.arm(RETRY_MS).await;
        }
        "Stopping…".into()
    }

    fn describe(&self, cfg: &Config) -> Result<String> {
        let mut lines = Vec::new();
        match self.turn() {
            Some(turn) => {
                let seconds = now_ms().saturating_sub(turn.started_at_ms) / 1000;
                lines.push(format!("Working for {seconds}s."));
                let waiting = self.store.approvals()?.len();
                if waiting > 0 {
                    lines.push(format!("Waiting for {waiting} answer(s) from you."));
                }
            }
            None => lines.push("Idle.".into()),
        }
        let used = self.store.usage(today())?;
        lines.push(match cfg.daily_token_budget {
            0 => format!("Tokens used today: {used}."),
            budget => format!("Tokens used today: {used} of {budget}."),
        });
        Ok(lines.join("\n"))
    }

    // ----- Getting a turn to the runner -----

    /// Starts the shared MCP container, or keeps it up, without waiting for it.
    fn warm_mcp(&self) {
        let Ok(namespace) = self.env.durable_object(MCP_BINDING) else {
            return;
        };
        self.state.wait_until(async move {
            if let Ok(stub) = namespace.get_by_name(MCP_INSTANCE) {
                let _ = stub.fetch_with_str(&format!("http://mcp{WARM_PATH}")).await;
            }
        });
    }

    async fn drive(self: &Rc<Self>) {
        if std::mem::replace(&mut self.live.borrow_mut().driving, true) {
            return;
        }
        let outcome = self.drive_once().await;
        self.live.borrow_mut().driving = false;
        let Err(error) = outcome else { return };

        console_error!("could not reach the runner: {error}");
        let failures = self.update(|session| {
            session.turn.as_mut().map(|turn| {
                turn.failures += 1;
                turn.failures
            })
        });
        match failures {
            Some(failures) if failures >= MAX_DRIVE_FAILURES => {
                let message = format!(
                    "⚠️ I could not start the agent: {}",
                    render::escape(&error.to_string())
                );
                self.abandon_turn(&message).await;
            }
            Some(_) => self.arm(RETRY_MS).await,
            None => {}
        }
    }

    async fn drive_once(self: &Rc<Self>) -> Result<()> {
        if self.turn().is_none() {
            return Ok(());
        }
        let cfg = self.cfg()?;
        let host = self.host()?;
        if let Host::Container(container) = &host
            && !container.running()
        {
            self.start_container(&cfg, container)?;
        }
        if self.live.borrow().socket.is_none() {
            self.establish(&cfg, &host).await?;
        }

        // Read the turn again: `/stop` may have ended it during the awaits above. From
        // here to the end nothing awaits, so the turn cannot change under this either.
        let Some(turn) = self.turn() else {
            return Ok(());
        };
        // The runner ignores a turn it already has, so repeating this is harmless.
        let handed_over = self.send(&ToRunner::UserMessage {
            turn_id: turn.id,
            text: turn.text,
        });
        if !handed_over {
            return Err(error("the control connection dropped"));
        }
        if turn.stop_requested {
            self.send(&ToRunner::Interrupt);
        }
        self.update(|session| {
            if let Some(turn) = &mut session.turn {
                turn.delivered = true;
                turn.failures = 0;
            }
        });
        Ok(())
    }

    /// Starts a container for this session. Returns once the start is accepted; whatever
    /// the new container still needs is recorded for [`Self::establish`], so it happens
    /// however many attempts that takes.
    fn start_container(&self, cfg: &Config, container: &Container) -> Result<()> {
        let (name, claude_session_id) = {
            let session = self.session.borrow();
            (session.name.clone(), session.claude_session_id.clone())
        };
        let expires_at = now_ms() / 1000 + TOKEN_TTL_SECS;
        let claims = Claims {
            session: name,
            expires_at,
        };
        let token = token::sign(cfg.token_secret.as_bytes(), &claims);
        let mut vars = vec![
            (runner_env::WORKER_ORIGIN, cfg.public_origin.as_str()),
            (runner_env::SESSION_TOKEN, token.as_str()),
            (runner_env::CLAUDE_SESSION_ID, claude_session_id.as_str()),
        ];
        if let Some(model) = &cfg.model {
            vars.push((runner_env::MODEL, model));
        }
        if let Some(model) = &cfg.small_model {
            vars.push((runner_env::SMALL_MODEL, model));
        }
        container_ext::start(container, &vars)?;

        // Two UUIDs give 244 random bits, well over the runner's minimum length.
        let secret = format!("{}{}", random_uuid(), random_uuid());
        self.update(|session| {
            session.control_secret = secret;
            session.token_expires_at = expires_at;
            session.needs_restore = session.has_state;
            // A new runner counts its frames from one again.
            session.last_seq = 0;
        });
        self.live.borrow_mut().socket = None;
        Ok(())
    }

    /// Takes a runner that exists to one that is claimed, has this session's files and
    /// is connected. Every step can be repeated, so a failure anywhere is retried from
    /// the top by the next `drive`.
    async fn establish(self: &Rc<Self>, cfg: &Config, host: &Host) -> Result<()> {
        if let Host::Container(container) = host {
            self.wait_until_ready(cfg, container).await?;
        }
        match self.claim().await? {
            Claim::Held => {}
            Claim::Fresh => {
                // The runner's own word that it is new. `start_container` records the
                // same thing, but a runner can also be replaced without this object
                // starting it, as a hand-started one is.
                self.update(|session| session.needs_restore = session.has_state);
                // Prompts the previous runner was waiting on died with it.
                self.close_approvals(&Bot::new(cfg), false, "The agent was restarted.")
                    .await;
            }
            Claim::Lost => {
                // Nothing can talk to that process again. Only this warrants throwing
                // the container away; a request that merely failed is retried.
                self.live.borrow_mut().socket = None;
                host.stop().await?;
                return Err(error(
                    "the runner belongs to an earlier start and had to be replaced",
                ));
            }
        }
        let needs_restore = self.session.borrow().needs_restore;
        if needs_restore {
            self.restore_state().await.map_err(|cause| {
                error(format!(
                    "the earlier conversation could not be restored ({cause}). \
                     Send /new to start a fresh one"
                ))
            })?;
            self.update(|session| session.needs_restore = false);
        }
        self.connect(host).await
    }

    async fn wait_until_ready(&self, cfg: &Config, container: &Container) -> Result<()> {
        let port = container.get_tcp_port(RUNNER_PORT)?;
        // Kept for the error: the platform's own words say why a container is not
        // coming up, for instance that none can be provided right after a deployment.
        let mut last = "its health check never answered".to_owned();
        let mut ready = false;
        for _ in 0..READY_ATTEMPTS {
            match port.fetch("http://runner/healthz", None).await {
                Ok(response) if response.status_code() == 200 => {
                    ready = true;
                    break;
                }
                Ok(response) => {
                    last = format!("its health check answered {}", response.status_code());
                }
                Err(error) => last = error.to_string(),
            }
            Delay::from(Duration::from_millis(READY_POLL_MS)).await;
        }
        if !ready {
            return Err(error(format!(
                "the agent container did not become ready ({last})"
            )));
        }
        // Not kept across a restart of this object, hence set on every connection. Best
        // effort: older runtimes may not have the call.
        let keep_alive = cfg.idle_ttl_ms + KEEP_ALIVE_MARGIN_MS;
        let _ = container_ext::keep_alive_for(container, keep_alive).await;
        Ok(())
    }

    /// Sets the runner's control secret, which it accepts once; repeating the same one
    /// is fine.
    async fn claim(&self) -> Result<Claim> {
        if self.session.borrow().control_secret.is_empty() {
            // Only an external runner gets here: containers are given one at start.
            let secret = format!("{}{}", random_uuid(), random_uuid());
            self.update(|session| session.control_secret = secret);
        }
        let secret = self.session.borrow().control_secret.clone();
        let response = self
            .runner(Method::Post, "/claim", Some(JsValue::from_str(&secret)))
            .await?;
        match response.status_code() {
            201 => Ok(Claim::Fresh),
            409 => Ok(Claim::Lost),
            _ => expect_ok(response, "claiming the runner")
                .await
                .map(|_| Claim::Held),
        }
    }

    async fn restore_state(&self) -> Result<()> {
        let bucket = self.env.bucket(STATE_BUCKET)?;
        let Some(object) = bucket.get(self.state_key()).execute().await? else {
            return Ok(());
        };
        let Some(body) = object.body() else {
            return Ok(());
        };
        let body = match body.response_body()? {
            ResponseBody::Stream(stream) => JsValue::from(stream),
            ResponseBody::Body(bytes) => Uint8Array::from(bytes.as_slice()).into(),
            ResponseBody::Empty => return Ok(()),
        };
        let response = self.runner(Method::Post, "/bootstrap", Some(body)).await?;
        expect_ok(response, "restoring session state")
            .await
            .map(drop)
    }

    async fn connect(self: &Rc<Self>, host: &Host) -> Result<()> {
        let headers = Headers::new();
        headers.set("upgrade", "websocket")?;
        let secret = self.session.borrow().control_secret.clone();
        headers.set("authorization", &format!("Bearer {secret}"))?;
        let mut init = RequestInit::new();
        init.with_headers(headers);
        let response = host.fetch("/ws", init).await?;
        let socket = response
            .websocket()
            .ok_or_else(|| error("the runner refused the control connection"))?;
        socket.accept()?;
        self.live.borrow_mut().socket = Some(socket.clone());
        let this = self.clone();
        wasm_bindgen_futures::spawn_local(async move { this.read_frames(socket).await });
        Ok(())
    }

    // ----- Frames from the runner -----

    async fn read_frames(self: Rc<Self>, socket: WebSocket) {
        match socket.events() {
            Ok(mut events) => {
                while let Some(Ok(WebsocketEvent::Message(message))) = events.next().await {
                    let Some(text) = message.text() else { continue };
                    match serde_json::from_str::<FromRunner>(&text) {
                        Ok(frame) => self.on_frame(frame).await,
                        Err(error) => console_warn!("unreadable frame from the runner: {error}"),
                    }
                }
            }
            Err(error) => console_error!("listening to the runner failed: {error}"),
        }
        // Closed or broken. Forget this socket unless a newer one already replaced it.
        {
            let mut live = self.live.borrow_mut();
            if live.socket.as_ref() == Some(&socket) {
                live.socket = None;
            }
        }
        if self.turn().is_some() {
            self.arm(RETRY_MS).await;
        }
    }

    async fn on_frame(self: &Rc<Self>, frame: FromRunner) {
        let seq = match &frame {
            FromRunner::Heartbeat => return,
            FromRunner::Hello { last_seq, .. } => {
                // A runner that has sent less than we have seen is a new one.
                let after_seq = self.update(|session| {
                    if *last_seq < session.last_seq {
                        session.last_seq = 0;
                    }
                    session.last_seq
                });
                self.send(&ToRunner::Resume { after_seq });
                return;
            }
            frame => frame.seq().unwrap_or_default(),
        };
        if seq <= self.session.borrow().last_seq {
            return;
        }

        // Handling never leaves a frame worth seeing again: a prompt that cannot be
        // shown is answered for the user, and a finished turn is closed whatever part of
        // reporting it failed. So the frame counts as done either way.
        let streamed = matches!(frame, FromRunner::Event { .. });
        self.handle(frame).await;
        self.session.borrow_mut().last_seq = seq;
        let due = {
            let mut live = self.live.borrow_mut();
            live.unacked_events = if streamed {
                live.unacked_events + 1
            } else {
                ACK_EVERY
            };
            live.unacked_events >= ACK_EVERY
        };
        if due {
            self.live.borrow_mut().unacked_events = 0;
            self.update(|_| ());
            self.send(&ToRunner::Ack { seq });
        }
    }

    async fn handle(self: &Rc<Self>, frame: FromRunner) {
        let current = self.turn().map(|turn| turn.id);
        match frame {
            FromRunner::Event { turn_id, event, .. } if Some(&turn_id) == current.as_ref() => {
                if self.live.borrow_mut().status.observe(&event) {
                    self.show_status().await;
                }
            }
            FromRunner::PermissionRequest {
                turn_id,
                request_id,
                tool_name,
                input,
                ..
            } => {
                let asked = if Some(&turn_id) == current.as_ref() {
                    let id = request_id.clone();
                    self.ask(turn_id, id, tool_name, input).await
                } else {
                    Err(error("the request belongs to a turn that is over"))
                };
                if let Err(error) = asked {
                    // Claude is blocked on this, so it must get an answer either way.
                    console_error!("a permission request could not be put to the user: {error}");
                    self.send(&ToRunner::PermissionDecision {
                        request_id,
                        decision: Decision::Deny {
                            message: "The user could not be asked.".into(),
                        },
                    });
                }
            }
            FromRunner::TurnDone {
                turn_id, result, ..
            } if Some(&turn_id) == current.as_ref() => self.finish_turn(result).await,
            _ => {}
        }
    }

    /// Updates the progress message, at most once per interval.
    async fn show_status(&self) {
        let Some(message_id) = self.turn().and_then(|turn| turn.status_message_id) else {
            return;
        };
        let now = now_ms();
        let html = {
            let mut live = self.live.borrow_mut();
            if now.saturating_sub(live.last_status_edit_ms) < STATUS_EDIT_INTERVAL_MS {
                return;
            }
            live.last_status_edit_ms = now;
            live.status.render(render::WORKING)
        };
        if let Ok(cfg) = self.cfg() {
            let bot = Bot::new(&cfg);
            let _ = bot
                .edit_message(self.chat_id(), message_id, &html, None)
                .await;
        }
    }

    /// Gives the progress message its last form when a turn is over. It stays in the chat
    /// as the record of what was done; one that never got past "Working…" is removed.
    async fn close_status(&self, bot: &Bot, turn: &Turn, heading: &str) {
        let Some(message_id) = turn.status_message_id else {
            return;
        };
        let html = {
            let live = self.live.borrow();
            (!live.status.is_empty()).then(|| live.status.render(heading))
        };
        let chat_id = self.chat_id();
        let _ = match html {
            Some(html) => bot.edit_message(chat_id, message_id, &html, None).await,
            None => bot.delete_message(chat_id, message_id).await,
        };
    }

    // ----- Permission prompts and clarifying questions -----

    /// Shows a prompt and records it as waiting for the user, unless the user already
    /// answered "Always" to the same thing in this conversation.
    async fn ask(
        &self,
        turn_id: String,
        request_id: String,
        tool_name: String,
        input: Value,
    ) -> Result<()> {
        if let Some(key) = remember::key(&tool_name, &input)
            && self.store.is_remembered(&key)?
        {
            self.send(&ToRunner::PermissionDecision {
                request_id,
                decision: Decision::Allow {
                    updated_input: None,
                },
            });
            return Ok(());
        }
        let cfg = self.cfg()?;
        let (html, keyboard) = if tool_name == ASK_USER_QUESTION {
            render::question_prompt(&request_id, &input, 0)
                .ok_or_else(|| error("a question without options cannot be shown"))?
        } else {
            render::permission_prompt(&request_id, &tool_name, &input)
        };
        let bot = Bot::new(&cfg);
        let message_id = bot
            .send_message(self.chat_id(), &html, Some(&keyboard))
            .await?;
        self.store.put_approval(&Approval {
            request_id,
            turn_id,
            tool_name,
            input,
            message_id,
            deadline_ms: now_ms() + cfg.approval_timeout_ms,
            answers: Default::default(),
        })
    }

    async fn on_callback(
        self: &Rc<Self>,
        cfg: &Config,
        bot: &Bot,
        query: CallbackQuery,
    ) -> Result<()> {
        let callback = query.data.as_deref().and_then(Callback::parse);
        let approval = match &callback {
            Some(callback) => self.store.approval(callback.request_id())?,
            None => None,
        };
        let (Some(callback), Some(mut approval)) = (callback, approval) else {
            return bot
                .answer_callback(&query.id, Some("This is no longer open."))
                .await;
        };
        let chat_id = self.chat_id();

        let always = matches!(
            callback,
            Callback::Permission {
                choice: render::Choice::AllowAlways,
                ..
            }
        );
        let (decision, outcome) = match callback {
            Callback::Permission { choice, .. } => {
                let decision = match choice {
                    render::Choice::Deny => Decision::Deny {
                        message: "The user declined.".into(),
                    },
                    render::Choice::Allow | render::Choice::AllowAlways => Decision::Allow {
                        updated_input: None,
                    },
                };
                let outcome = render::permission_outcome(&approval.tool_name, choice);
                (decision, outcome)
            }
            Callback::Answer {
                question, option, ..
            } => {
                let Some((text, label)) =
                    render::question_answer(&approval.input, question, option)
                else {
                    return bot.answer_callback(&query.id, None).await;
                };
                approval.answers.insert(text, label.into());
                let request_id = approval.request_id.clone();
                let next = question + 1;
                if let Some((html, keyboard)) =
                    render::question_prompt(&request_id, &approval.input, next)
                {
                    // More to ask: the same message turns into the next question.
                    self.store.put_approval(&approval)?;
                    let _ = bot.answer_callback(&query.id, None).await;
                    return bot
                        .edit_message(chat_id, approval.message_id, &html, Some(&keyboard))
                        .await;
                }
                // In the order the questions were asked, not the map's.
                let chosen: Vec<_> = approval.input["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|asked| approval.answers.get(asked["question"].as_str()?)?.as_str())
                    .collect();
                let outcome = format!("<b>✔️ {}</b>", render::escape(&chosen.join(", ")));
                let decision = Decision::Allow {
                    updated_input: Some(render::answered_questions(
                        &approval.input,
                        &approval.answers,
                    )),
                };
                (decision, outcome)
            }
        };

        // The answer has to reach the runner before the prompt is marked as answered.
        // If it cannot be delivered now, the prompt stays open and can be pressed again.
        let request_id = approval.request_id.clone();
        if let Err(error) = self.pass_decision(cfg, request_id, decision).await {
            console_error!("a decision could not be delivered: {error}");
            self.arm(RETRY_MS).await;
            let retry = "I couldn't reach the agent just now. Please press again in a moment.";
            return bot.answer_callback(&query.id, Some(retry)).await;
        }
        let _ = bot.answer_callback(&query.id, None).await;
        self.store.delete_approval(&approval.request_id)?;
        if always && let Some(key) = remember::key(&approval.tool_name, &approval.input) {
            self.store.remember(&key)?;
        }
        let (prompt, _) = self.prompt_of(&approval);
        let html = format!("{prompt}\n\n{outcome}");
        bot.edit_message(chat_id, approval.message_id, &html, None)
            .await
    }

    /// Gets a decision to the runner, reconnecting first if this object was evicted
    /// since the prompt was shown.
    async fn pass_decision(
        self: &Rc<Self>,
        cfg: &Config,
        request_id: String,
        decision: Decision,
    ) -> Result<()> {
        if self.live.borrow().socket.is_none() {
            let host = self.host()?;
            if !host.running() {
                return Err(error("the agent is not running"));
            }
            self.establish(cfg, &host).await?;
        }
        let sent = self.send(&ToRunner::PermissionDecision {
            request_id,
            decision,
        });
        if sent {
            Ok(())
        } else {
            Err(error("the control connection dropped"))
        }
    }

    /// The text an approval's message was sent with, to redraw it without its buttons.
    fn prompt_of(&self, approval: &Approval) -> (String, Value) {
        if approval.tool_name == ASK_USER_QUESTION {
            let last = approval.input["questions"].as_array().map_or(0, Vec::len);
            render::question_prompt(
                &approval.request_id,
                &approval.input,
                last.saturating_sub(1),
            )
            .unwrap_or_default()
        } else {
            render::permission_prompt(&approval.request_id, &approval.tool_name, &approval.input)
        }
    }

    /// Closes every open prompt, telling Claude `reason` where a runner still listens.
    async fn close_approvals(&self, bot: &Bot, only_expired: bool, reason: &str) {
        let now = now_ms();
        let Ok(approvals) = self.store.approvals() else {
            return;
        };
        for approval in approvals {
            if only_expired && approval.deadline_ms > now {
                continue;
            }
            self.send(&ToRunner::PermissionDecision {
                request_id: approval.request_id.clone(),
                decision: Decision::Deny {
                    message: reason.to_owned(),
                },
            });
            let _ = self.store.delete_approval(&approval.request_id);
            let (prompt, _) = self.prompt_of(&approval);
            let html = format!("{prompt}\n\n<b>⌛ Expired</b>");
            let _ = bot
                .edit_message(self.chat_id(), approval.message_id, &html, None)
                .await;
        }
    }

    // ----- Finishing a turn -----

    /// Ends the open turn and schedules the idle shutdown. Nothing in here can fail, so
    /// a turn that reaches this point is always closed.
    async fn close_turn(&self, idle_ttl_ms: u64) {
        self.update(|session| {
            session.turn = None;
            session.idle_deadline_ms = now_ms() + idle_ttl_ms;
        });
        self.arm(idle_ttl_ms).await;
    }

    async fn finish_turn(&self, result: TurnResult) {
        let Some(turn) = self.turn() else { return };
        let idle_ttl_ms = match self.cfg() {
            Ok(cfg) => {
                self.report_turn(&cfg, &turn, &result).await;
                cfg.idle_ttl_ms
            }
            Err(error) => {
                console_error!("the end of a turn could not be reported: {error}");
                WATCHDOG_MS
            }
        };
        // Only now may the next message start a turn: the files had to be read while
        // nothing was changing them.
        self.close_turn(idle_ttl_ms).await;
    }

    /// Everything owed to the user when a turn ends. Each part is attempted whatever
    /// happened to the others, and what could not be done is said at the end.
    async fn report_turn(&self, cfg: &Config, turn: &Turn, result: &TurnResult) {
        let bot = Bot::new(cfg);
        let answer = result
            .text
            .as_deref()
            .filter(|text| !text.trim().is_empty());
        let (heading, messages) = match answer {
            // A turn that finished is answered even if a stop was on its way.
            Some(text) if !result.is_error => (render::DONE, render::answer(text)),
            _ if turn.stop_requested => (render::STOPPED, vec!["⏹ Stopped.".to_owned()]),
            None if !result.is_error => (render::DONE, vec!["(no answer)".to_owned()]),
            detail => {
                let mut message = format!(
                    "⚠️ The agent stopped early ({}).",
                    render::escape(&result.subtype)
                );
                if let Some(detail) = detail {
                    message.push_str(&format!("\n<pre>{}</pre>", render::escape(detail)));
                }
                (render::FAILED, vec![message])
            }
        };
        self.close_status(&bot, turn, heading).await;
        let mut problems = Vec::new();
        for message in &messages {
            if let Err(error) = self.deliver(&bot, message).await {
                console_error!("sending the answer failed: {error}");
                problems.push("Part of my answer could not be delivered.".to_owned());
                break;
            }
        }

        self.close_approvals(&bot, false, "The turn is over.").await;
        for artifact in &result.artifacts {
            if let Err(error) = self.deliver_artifact(&bot, turn, artifact).await {
                console_error!("delivering {} failed: {error}", artifact.path);
                problems.push(format!(
                    "I could not send {}.",
                    render::escape(&artifact.path)
                ));
            }
        }
        if let Err(error) = self.save_state().await {
            console_error!("saving session state failed: {error}");
            problems.push("I could not save this conversation; it may be forgotten.".to_owned());
        }
        if !problems.is_empty() {
            let _ = self
                .deliver(&bot, &format!("⚠️ {}", problems.join("\n")))
                .await;
        }
    }

    /// Gives up on a turn the runner will never finish.
    async fn abandon_turn(&self, message: &str) {
        let turn = self.update(|session| session.turn.take());
        if let Ok(cfg) = self.cfg() {
            let bot = Bot::new(&cfg);
            if let Some(turn) = &turn {
                self.close_status(&bot, turn, render::FAILED).await;
            }
            self.close_approvals(&bot, false, "The turn was abandoned.")
                .await;
            let _ = self.deliver(&bot, message).await;
        }
        // Whatever container is left is stopped by the idle branch of the alarm.
        self.update(|session| session.idle_deadline_ms = now_ms());
        self.arm(RETRY_MS).await;
    }

    /// Copies an output file to R2 and, if it is small enough, into the chat.
    async fn deliver_artifact(&self, bot: &Bot, turn: &Turn, artifact: &Artifact) -> Result<()> {
        let mut url = Url::parse("http://runner/files/out")?;
        url.path_segments_mut()
            .map_err(|()| error("bad artifact URL"))?
            .extend(artifact.path.split('/'));
        let response = self.runner(Method::Get, url.path(), None).await?;
        let mut response = expect_ok(response, "reading an output file").await?;

        let bucket = self.env.bucket(STATE_BUCKET)?;
        let name = self.session.borrow().name.clone();
        let key = format!("artifacts/{name}/{}/{}", turn.id, artifact.path);
        let file_name = artifact.path.rsplit('/').next().unwrap_or(&artifact.path);
        let chat_id = self.chat_id();

        if artifact.size > TELEGRAM_FILE_LIMIT {
            let stream = FixedLengthStream::wrap(response.stream()?, artifact.size);
            bucket.put(key.clone(), stream).execute().await?;
            let note = format!(
                "📎 <code>{}</code> is too large to send here ({} MB). It is stored as <code>{}</code>.",
                render::escape(file_name),
                artifact.size / (1024 * 1024),
                render::escape(&key),
            );
            return bot.send_message(chat_id, &note, None).await.map(drop);
        }

        // Stored first: the container's copy is gone once it stops, so a file Telegram
        // will not take must still exist somewhere.
        let bytes = response.bytes().await?;
        let stored = bucket.put(key.clone(), bytes.clone()).execute().await;
        match (bot.send_document(chat_id, file_name, &bytes).await, stored) {
            (Ok(()), Ok(_)) => Ok(()),
            (Ok(()), Err(error)) => {
                // The user has the file; only the archive copy is missing.
                console_error!("storing {key} failed: {error}");
                Ok(())
            }
            (Err(error), Ok(_)) => {
                console_error!("sending {file_name} failed: {error}");
                let note = format!(
                    "📎 <code>{}</code> could not be sent here. It is stored as <code>{}</code>.",
                    render::escape(file_name),
                    render::escape(&key),
                );
                bot.send_message(chat_id, &note, None).await.map(drop)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    /// Stores the runner's state archive, so the next container can pick up from here.
    async fn save_state(&self) -> Result<()> {
        if self.session.borrow().needs_restore {
            return Err(error(
                "this container never received the saved state; keeping the saved copy",
            ));
        }
        let response = self.runner(Method::Get, "/state", None).await?;
        let mut response = expect_ok(response, "exporting session state").await?;
        let length = response
            .headers()
            .get("content-length")?
            .and_then(|length| length.parse().ok())
            .ok_or_else(|| error("the state archive has no length"))?;
        let stream = FixedLengthStream::wrap(response.stream()?, length);
        let bucket = self.env.bucket(STATE_BUCKET)?;
        bucket.put(self.state_key(), stream).execute().await?;
        self.update(|session| session.has_state = true);
        Ok(())
    }

    // ----- The alarm -----

    /// An error returned from here makes [`SessionDO::alarm`] set the alarm again, so
    /// every early return is a retry, not an end.
    async fn on_alarm(self: &Rc<Self>) -> Result<()> {
        let cfg = self.cfg()?;
        let now = now_ms();
        let Some(turn) = self.turn() else {
            // Idle: stop the warm container once its time is up. A deployment without
            // containers has nothing to stop.
            if let Ok(host) = self.host()
                && host.running()
            {
                let deadline = self.session.borrow().idle_deadline_ms;
                if now >= deadline {
                    self.live.borrow_mut().socket = None;
                    host.stop().await?;
                } else {
                    self.arm(deadline - now).await;
                }
            }
            return Ok(());
        };

        let bot = Bot::new(&cfg);
        let running_for = now.saturating_sub(turn.started_at_ms);
        if running_for > cfg.turn_timeout_ms + STOP_GRACE_MS {
            // The turn ends here whether or not the container can be stopped right now;
            // abandoning it leaves that to the idle branch above.
            self.live.borrow_mut().socket = None;
            self.abandon_turn("⚠️ The task ran too long and did not stop, so I ended it.")
                .await;
            return Ok(());
        }
        if running_for > cfg.turn_timeout_ms && !turn.stop_requested {
            self.update(|session| {
                if let Some(turn) = &mut session.turn {
                    turn.stop_requested = true;
                }
            });
            self.send(&ToRunner::Interrupt);
            let _ = bot
                .send_message(self.chat_id(), "⏱ This is taking too long; stopping.", None)
                .await;
        }
        self.close_approvals(&bot, true, "The user did not answer in time.")
            .await;

        self.warm_mcp();
        self.arm(WATCHDOG_MS).await;
        self.drive().await;
        Ok(())
    }
}
