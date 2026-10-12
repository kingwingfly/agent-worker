//! One chat's session: a Durable Object that owns the agent container, the conversation
//! state and everything shown in Telegram.
//!
//! A session is that of a chat the bot is in, or of one user's summons from a chat it is
//! a guest in ([`Session::guest`]). A guest may send one message per summons and change
//! it afterwards, so there a turn's progress, its prompts and its answer are all shown in
//! that one message, by turns.
//!
//! All durable state is in [`Session`] and the approvals table. Work is pushed forward by
//! [`Inner::drive`], which looks at that state and does whatever is missing (start the
//! container, restore its files, connect, hand over the message). It is safe to call at
//! any time, and an alarm calls it periodically while a turn is open, so a Durable Object
//! that was evicted or a container that died mid-turn is picked up again.

use std::{cell::RefCell, rc::Rc, time::Duration};

use agent_proto::{
    ASK_USER_QUESTION, Artifact, Decision, Effort, FromRunner, RUNNER_PORT, ToRunner, TurnResult,
    env as runner_env,
    token::{self, Claims},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasm_bindgen::{JsValue, UnwrapThrowExt};
use worker::{
    Bucket, Container, Delay, DurableObject, Env, Fetch, FixedLengthStream, Headers, Method,
    Request, RequestInit, Response, ResponseBody, Result, State, Url, WebSocket, WebsocketEvent,
    console_error, console_warn, durable_object, js_sys::Uint8Array, wasm_bindgen_futures,
};

use crate::{
    bot::Bot,
    calls::{self, Admission, Refusal, Standing},
    config::Config,
    container_ext,
    mcp_host::{MCP_BINDING, MCP_INSTANCE, WARM_PATH},
    platform::{error, expect_ok, now_ms, random_uuid},
    remember,
    render::{self, Callback, Pick, Status},
    store::{Approval, Checkpoint, Store},
    update::{self, CallbackQuery, Message, StoppedGeneration, Update},
};

pub const SESSION_BINDING: &str = "SESSION";
/// Carries the Durable Object's name, which it cannot always read from its own id.
pub const NAME_HEADER: &str = "x-session-name";
/// Requests to the object: a Telegram update, tokens spent as seen by the LLM proxy, or
/// a proxy asking whether a session token may still be acted on.
pub const UPDATE_PATH: &str = "/update";
pub const USAGE_PATH: &str = "/usage";
pub const ADMIT_PATH: &str = "/admit";

const STATE_BUCKET: &str = "STATE";
const SESSION_KEY: &str = "session";

const WATCHDOG_MS: u64 = 30_000;
const RETRY_MS: u64 = 2_000;
const STATUS_EDIT_INTERVAL_MS: u64 = 2_000;
/// The least time between two drafts of the text being written. The runner paces what it
/// sends; this keeps what piled up behind a slow call from going out all at once.
const DRAFT_INTERVAL_MS: u64 = 500;
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
/// How long Telegram is offered the last form of a guest's reply before it is given up.
const UNSENT_KEPT_MS: u64 = 15 * 60_000;
/// The pause before offering it again after a refusal that did not say how long to wait.
const UNSENT_RETRY_MS: u64 = 15_000;
/// How often it is offered while Telegram refuses it for another reason than too many
/// calls, such as a reply that was deleted. Waiting does not mend that.
const UNSENT_TRIES: u32 = 4;
/// Checkpoints kept for a conversation. The oldest of them is only what the next one
/// started from, so one turn fewer can be undone.
const CHECKPOINTS_KEPT: usize = 10;
/// Conversations kept for a chat, the one it is in included.
const CONVERSATIONS_KEPT: usize = 10;

const HELP: &str = "Send me a message and I'll get to work.\n\n\
    /new — start a fresh conversation\n\
    /resume — go back to an earlier conversation\n\
    /rewind — go back to before one of your messages\n\
    /effort — how much work I put into an answer\n\
    /stop — stop what I'm doing\n\
    /status — what I'm doing and today's usage";

/// What a guest says about itself. Commands reach it like anything else: in a message
/// that mentions it.
const GUEST_HELP: &str = "Mention me with what you want, or reply to one of my messages, \
    and I'll answer here in one message. What you ask me in this chat is a conversation of \
    your own.\n\n\
    /new — start a fresh conversation\n\
    /resume — go back to an earlier conversation\n\
    /rewind — go back to before one of your messages\n\
    /effort — how much work I put into an answer\n\
    /stop — stop what I'm doing\n\
    /status — what I'm doing and today's usage";
/// For a press that finds nothing to answer in a chat the bot is in, and where it is a
/// guest. There the press may also be someone else's than the user's who was asked.
const GONE: &str = "This is no longer open.";
const GUEST_GONE: &str = "Only whoever asked me can answer this, while it is open.";

/// Where what the bot says to one incoming message goes.
enum Reply<'a> {
    /// Into the chat, as a message of its own.
    Chat,
    /// Into the one reply a guest summons may get, by the id of the summons.
    Guest(&'a str),
}

/// A message the bot sent and can change afterwards.
enum Said {
    /// One in the chat, by its id there.
    Chat(i64),
    /// The reply to a guest summons, by the id it is changed by.
    Guest(String),
}

/// What there was to stop.
#[derive(Clone, Copy)]
enum Stopped {
    /// No turn.
    Nothing,
    /// A turn the runner had not been given yet, which ended there and then.
    Turn,
    /// A turn the runner is working on. It has been told, and the turn ends when the
    /// runner says so.
    Runner,
}

impl Stopped {
    fn said(self) -> &'static str {
        match self {
            Self::Nothing => "Nothing is running.",
            Self::Turn => "⏹ Stopped.",
            Self::Runner => "Stopping…",
        }
    }
}

/// What became of an output file. Whichever it is, the file is in R2 by then.
enum Filed {
    /// It is in the chat.
    Sent,
    /// It is larger than what is uploaded to a chat, and kept under this key.
    TooLarge(String),
    /// Telegram did not take it. It is kept under this key.
    Refused(String),
}

/// The last part of an output file's path, which is what it is called in a chat.
fn file_name(artifact: &Artifact) -> &str {
    artifact.path.rsplit('/').next().unwrap_or(&artifact.path)
}

/// What a chat is told about a file that is kept but was not sent to it.
fn kept_note(artifact: &Artifact, filed: &Filed) -> Option<String> {
    let name = render::escape(file_name(artifact));
    match filed {
        Filed::Sent => None,
        Filed::TooLarge(key) => Some(format!(
            "📎 <code>{name}</code> is too large to send here ({} MB). It is stored as <code>{}</code>.",
            artifact.size / (1024 * 1024),
            render::escape(key),
        )),
        Filed::Refused(key) => Some(format!(
            "📎 <code>{name}</code> could not be sent here. It is stored as <code>{}</code>.",
            render::escape(key),
        )),
    }
}

/// What a guest's reply says about a file. It went to another chat than the one the
/// reply is in, or to none: the bot may write only to people who opened a chat with it.
fn guest_file_note(artifact: &Artifact, filed: Option<&Filed>) -> String {
    let name = render::escape(file_name(artifact));
    match filed {
        Some(Filed::Sent) => {
            format!("📎 <code>{name}</code> went to the private chat of whoever asked.")
        }
        Some(Filed::TooLarge(_)) => format!(
            "📎 <code>{name}</code> is too large to send ({} MB).",
            artifact.size / (1024 * 1024)
        ),
        Some(Filed::Refused(_)) | None => format!(
            "📎 <code>{name}</code> could not be sent. Files go to the private chat with \
             me, which whoever asked has to open first."
        ),
    }
}

/// The command a message is, if it is one. It may arrive as `/stop@name_of_the_bot`.
fn command(text: &str) -> Option<&str> {
    text.split_whitespace()
        .next()
        .filter(|word| word.starts_with('/'))
        .map(|word| word.split('@').next().unwrap_or(word))
}

/// A name for the draft of a turn that begins now. Telegram takes any number but zero.
/// It is told apart from the draft of the turn before, whose Stop button can still be
/// pressed when the next turn runs.
fn new_draft_id() -> i64 {
    (now_ms() % i32::MAX as u64) as i64 + 1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Session {
    /// Durable Object name, e.g. `tg:12345`.
    name: String,
    chat_id: i64,
    /// Set for the session of a guest: the user whose summons from `chat_id` it answers.
    /// The bot is not a member of that chat, so nothing can be sent to it; all the bot
    /// has there is the reply each summons gets. `None` for a chat the bot is in.
    #[serde(default)]
    guest: Option<i64>,
    /// The chat is one user's with the bot, which is where the text being written can
    /// be shown as a draft.
    #[serde(default)]
    private: bool,
    claude_session_id: String,
    /// Secret the current container's runner was claimed with.
    control_secret: String,
    /// Unix seconds at which the current container's session token expires.
    token_expires_at: u64,
    /// A state archive for the conversation exists in R2.
    has_state: bool,
    /// The checkpoint that archive is: what the conversation was last saved as, and what
    /// a new container is restored from. Zero while `has_state` is set means a state
    /// saved before conversations had checkpoints.
    #[serde(default)]
    checkpoint: u64,
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
    /// The level `/effort` set, which every turn of this chat is answered at, in whatever
    /// conversation. `None` leaves it to the model's default.
    #[serde(default)]
    effort: Option<Effort>,
    /// What guests' replies are to say in the end and Telegram has not taken yet. A
    /// reply is all a guest's turn has, so its last form is kept until it is on show.
    #[serde(default)]
    unsent: Vec<Unsent>,
}

/// The last form of a guest's reply, on its way to Telegram.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Unsent {
    /// The reply, by the id it is changed by.
    reply: String,
    html: String,
    /// When the turn ended.
    since_ms: u64,
    /// It is not offered again before this.
    retry_at_ms: u64,
    /// How often Telegram has refused it.
    tries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Turn {
    id: String,
    text: String,
    started_at_ms: u64,
    /// The message that shows the turn's progress. It is edited as the turn goes on and
    /// stays in the chat when it is over.
    status_message_id: Option<i64>,
    /// Something was sent below the progress message since it was last shown, so it has
    /// scrolled up the chat.
    #[serde(default)]
    status_buried: bool,
    /// The message has been handed to the runner, so stopping means interrupting it.
    #[serde(default)]
    delivered: bool,
    #[serde(default)]
    stop_requested: bool,
    /// Consecutive failed attempts to get the turn to the runner.
    #[serde(default)]
    failures: u32,
    /// For a guest's turn, the reply its summons got: the one message all of the turn is
    /// shown in. It is taken out when the message gets its last form.
    #[serde(default)]
    guest_reply: Option<String>,
    /// Names the draft the text is shown in while Claude writes it. Zero for a turn
    /// without one: only a private chat has drafts.
    #[serde(default)]
    draft_id: i64,
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
    last_draft_ms: u64,
    /// A draft with text in it is on show.
    draft_shown: bool,
    /// Telegram did not take a draft, so the turn goes on without them.
    drafts_off: bool,
    /// Telegram asked for fewer calls until then. A guest's progress is left out for
    /// that long, so that what the wait is over for is the answer.
    quiet_until_ms: u64,
    unacked_events: u32,
    /// The chat is being moved to another conversation, or to another point in this one.
    switching: bool,
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
        if request.path() == ADMIT_PATH {
            let asked: Admission = request.json().await?;
            return match self.inner.refusal(asked)? {
                Some(refusal) => Response::error("refused", refusal.status()),
                None => Response::empty(),
            };
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

/// Where a conversation as it was after one of its turns is kept. Checkpoints are named
/// by the time they were saved at, so a name is never used twice.
fn checkpoint_key(name: &str, conversation: &str, at_ms: u64) -> String {
    format!("sessions/{name}/{conversation}/{at_ms}.tar.gz")
}

/// Where a chat's one saved state was kept before conversations had checkpoints.
fn legacy_state_key(name: &str) -> String {
    format!("sessions/{name}/state.tar.gz")
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

    /// Why a request that came with a valid session token is turned away, if it is: the
    /// token belongs to a container that is no longer this chat's, or it is a model call
    /// and today's tokens are used up.
    fn refusal(&self, asked: Admission) -> Result<Option<Refusal>> {
        let cfg = self.cfg()?;
        let host = self.host()?;
        let standing = Standing {
            // A runner started by hand was given no token by this object.
            issued: matches!(host, Host::Container(_))
                .then(|| self.session.borrow().token_expires_at),
            running: host.running(),
            used: self.store.usage(today())?,
            budget: cfg.daily_token_budget,
        };
        Ok(calls::refusal(asked, standing))
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

    /// Whether this is the session of a guest, see [`Session::guest`].
    fn is_guest(&self) -> bool {
        self.session.borrow().guest.is_some()
    }

    /// The chat files are sent to. A guest can send none where it was summoned, so its
    /// files go to the private chat of the user it answers, which has that user's id.
    fn files_chat(&self) -> i64 {
        let session = self.session.borrow();
        session.guest.unwrap_or(session.chat_id)
    }

    /// Says `html` in answer to an incoming message, and returns what it was sent as.
    async fn reply(
        &self,
        bot: &Bot,
        to: &Reply<'_>,
        html: &str,
        keyboard: Option<&Value>,
    ) -> Result<Said> {
        match to {
            Reply::Chat => bot
                .send_message(self.chat_id(), html, keyboard)
                .await
                .map(Said::Chat),
            Reply::Guest(query_id) => {
                let keyboard =
                    keyboard.map(|keyboard| render::for_guest(keyboard.clone(), self.chat_id()));
                bot.answer_guest(query_id, html, keyboard.as_ref())
                    .await
                    .map(Said::Guest)
            }
        }
    }

    /// Where the conversation's saved state is.
    fn state_key(&self) -> String {
        let session = self.session.borrow();
        match session.checkpoint {
            0 => legacy_state_key(&session.name),
            at_ms => checkpoint_key(&session.name, &session.claude_session_id, at_ms),
        }
    }

    /// Why nothing that replaces the conversation's files can be done right now.
    fn busy(&self) -> Option<&'static str> {
        if self.turn().is_some() {
            Some("Stop the current task with /stop first.")
        } else if self.live.borrow().switching {
            Some("One moment, I'm still switching. Try again.")
        } else {
            None
        }
    }

    async fn arm(&self, delay_ms: u64) {
        // There is one alarm for everything. Whatever else it is set for, it comes
        // back in time for a reply Telegram has still to take.
        let unsent = self
            .session
            .borrow()
            .unsent
            .iter()
            .map(|unsent| unsent.retry_at_ms)
            .min();
        let delay_ms = match unsent {
            Some(at_ms) => delay_ms.min(at_ms.saturating_sub(now_ms())),
            None => delay_ms,
        };
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
            self.bury_status();
            if self.session.borrow().name.is_empty() {
                self.update(|session| {
                    session.name = name.to_owned();
                    session.chat_id = message.chat.id;
                    session.claude_session_id = random_uuid();
                });
            }
            let private = message.chat.is_private();
            if self.session.borrow().private != private {
                self.update(|session| session.private = private);
            }
            let to = Reply::Chat;
            match message.text.as_deref().map(str::trim) {
                Some(text) if !text.is_empty() => match command(text) {
                    Some(command) => self.on_command(&cfg, &bot, &to, command, text).await,
                    None => self.begin_turn(&cfg, &bot, &to, text).await,
                },
                _ => bot
                    .send_message(message.chat.id, "I can only read text messages.", None)
                    .await
                    .map(drop),
            }
        } else if let Some(message) = update.guest_message {
            self.on_guest(&cfg, &bot, name, &message).await
        } else if let Some(query) = update.callback_query {
            self.on_callback(&cfg, &bot, query).await
        } else if let Some(stopped) = update.stopped_message_generation {
            self.on_draft_stopped(&cfg, &bot, &stopped).await
        } else {
            Ok(())
        }
    }

    /// A message that summons the bot in a chat it is not a member of. Whatever comes of
    /// it goes into the one reply the summons may get.
    async fn on_guest(
        self: &Rc<Self>,
        cfg: &Config,
        bot: &Bot,
        name: &str,
        message: &Message,
    ) -> Result<()> {
        // Without these there is nobody to answer, or no way to.
        let (Some(query_id), Some(caller)) =
            (message.guest_query_id.as_deref(), message.from.as_ref())
        else {
            return Ok(());
        };
        if self.session.borrow().name.is_empty() {
            self.update(|session| {
                session.name = name.to_owned();
                session.chat_id = message.chat.id;
                session.guest = Some(caller.id);
                session.claude_session_id = random_uuid();
            });
        }
        let to = Reply::Guest(query_id);
        let Some(text) = message.text.as_deref() else {
            let unread = "I can only read text messages.";
            return self.reply(bot, &to, unread, None).await.map(drop);
        };
        let text = render::without_mentions(text);
        let replied = message.replied();
        if let Some(command) = command(text) {
            return self.on_command(cfg, bot, &to, command, text).await;
        }
        if text.is_empty() && replied.is_none() {
            // Summoned and asked nothing.
            return self.reply(bot, &to, GUEST_HELP, None).await.map(drop);
        }
        let request = render::guest_request(text, replied);
        self.begin_turn(cfg, bot, &to, &request).await
    }

    async fn on_command(
        self: &Rc<Self>,
        cfg: &Config,
        bot: &Bot,
        to: &Reply<'_>,
        command: &str,
        text: &str,
    ) -> Result<()> {
        let guest = self.is_guest();
        let help = if guest { GUEST_HELP } else { HELP };
        let (reply, keyboard) = match command {
            "/start" | "/help" => (help.to_owned(), None),
            "/new" => (self.reset().await?, None),
            "/resume" => self.offer_resume()?,
            "/rewind" => self.offer_rewind()?,
            "/effort" => self.on_effort(text.split_whitespace().nth(1)),
            "/stop" => (self.stop(cfg, bot).await.said().to_owned(), None),
            "/status" => (self.describe(cfg)?, None),
            other => (
                format!("I don't know {}.\n\n{help}", render::escape(other)),
                None,
            ),
        };
        self.reply(bot, to, &reply, keyboard.as_ref())
            .await
            .map(drop)
    }

    async fn begin_turn(
        self: &Rc<Self>,
        cfg: &Config,
        bot: &Bot,
        to: &Reply<'_>,
        text: &str,
    ) -> Result<()> {
        let refusal = if self.turn().is_some() {
            Some("I'm still working on your previous message. Send /stop to cancel it.")
        } else if self.live.borrow().switching {
            Some("One moment, I'm still switching. Send that again.")
        } else if cfg.daily_token_budget > 0 && self.store.usage(today())? >= cfg.daily_token_budget
        {
            Some("Today's usage limit for this chat is used up. Try again tomorrow.")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            return self.reply(bot, to, refusal, None).await.map(drop);
        }

        // Record the turn before the first await, so a second message cannot slip in.
        self.update(|session| {
            session.turn = Some(Turn {
                id: random_uuid(),
                text: text.to_owned(),
                started_at_ms: now_ms(),
                status_message_id: None,
                status_buried: false,
                delivered: false,
                stop_requested: false,
                failures: 0,
                guest_reply: None,
                draft_id: if session.private { new_draft_id() } else { 0 },
            });
        });
        {
            let mut live = self.live.borrow_mut();
            live.status = Status::default();
            live.last_status_edit_ms = 0;
            live.last_draft_ms = 0;
            live.draft_shown = false;
            live.drafts_off = false;
        }
        let shown = self.reply(bot, to, render::WORKING, None).await;
        if let (Reply::Guest(_), Err(cause)) = (to, &shown) {
            // The reply is all a guest's turn can be shown in. A turn that could tell
            // nobody how it went is not begun.
            self.update(|session| session.turn = None);
            return Err(error(format!(
                "a guest summons could not be answered: {cause}"
            )));
        }
        self.update(|session| {
            if let Some(turn) = &mut session.turn {
                match shown {
                    Ok(Said::Chat(message_id)) => turn.status_message_id = Some(message_id),
                    Ok(Said::Guest(reply)) => turn.guest_reply = Some(reply),
                    Err(_) => {}
                }
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

    /// `/new`: begin a fresh conversation. The one left behind stays where `/resume`
    /// finds it.
    async fn reset(&self) -> Result<String> {
        if let Some(busy) = self.busy() {
            return Ok(busy.into());
        }
        let unkept = self
            .switch(|| {
                self.store.clear_approvals()?;
                self.store.forget_all()?;
                Ok(self.update(|session| {
                    // A state saved before conversations had checkpoints belongs to none
                    // that could be resumed, so it goes as it always did.
                    let unkept = (session.has_state && session.checkpoint == 0)
                        .then(|| legacy_state_key(&session.name));
                    session.claude_session_id = random_uuid();
                    session.has_state = false;
                    session.checkpoint = 0;
                    session.needs_restore = false;
                    session.last_seq = 0;
                    unkept
                }))
            })
            .await?;
        self.discard(unkept).await;
        Ok("Started a fresh conversation. /resume goes back to an earlier one.".into())
    }

    // ----- Earlier conversations, and earlier points in this one -----

    /// Stops the container, whose files are the conversation as it is now, and then
    /// makes `change` to what the session says the conversation is. No turn may begin in
    /// between: it would run on the files of one state and be saved as another.
    async fn switch<T>(&self, change: impl FnOnce() -> Result<T>) -> Result<T> {
        {
            let mut live = self.live.borrow_mut();
            live.switching = true;
            live.socket = None;
        }
        let stopped = match self.host() {
            Ok(host) => host.stop().await,
            Err(_) => Ok(()),
        };
        let outcome = stopped.and_then(|()| change());
        self.live.borrow_mut().switching = false;
        outcome
    }

    /// Deletes archives nothing refers to any more. One that cannot be deleted is only
    /// left behind, so this is nobody's error.
    async fn discard(&self, keys: impl IntoIterator<Item = String>) {
        let Ok(bucket) = self.env.bucket(STATE_BUCKET) else {
            return;
        };
        for key in keys {
            if let Err(error) = bucket.delete(&key).await {
                console_error!("deleting {key} failed: {error}");
            }
        }
    }

    /// `/resume`: the chat's other conversations, to pick one from.
    fn offer_resume(&self) -> Result<(String, Option<Value>)> {
        if let Some(busy) = self.busy() {
            return Ok((busy.into(), None));
        }
        let current = self.session.borrow().claude_session_id.clone();
        let saved = self.store.conversations()?;
        Ok(match render::resume_picker(&saved, &current, now_ms()) {
            Some((html, keyboard)) => (html, Some(keyboard)),
            None => (
                "There is no earlier conversation to go back to.".into(),
                None,
            ),
        })
    }

    /// `/rewind`: the user's recent messages in this conversation, to pick one from.
    fn offer_rewind(&self) -> Result<(String, Option<Value>)> {
        if let Some(busy) = self.busy() {
            return Ok((busy.into(), None));
        }
        let current = self.session.borrow().claude_session_id.clone();
        let kept = self.store.checkpoints(&current)?;
        Ok(match render::rewind_picker(&current, &kept, now_ms()) {
            Some((html, keyboard)) => (html, Some(keyboard)),
            None => ("There is nothing to rewind yet.".into(), None),
        })
    }

    /// A press on the `/resume` or `/rewind` list.
    async fn on_pick(&self, bot: &Bot, query: &CallbackQuery, pick: Pick) -> Result<()> {
        if let Some(busy) = self.busy() {
            return bot.answer_callback(&query.id, Some(busy)).await;
        }
        let done = match &pick {
            Pick::Resume { conversation } => self.resume(conversation).await?,
            Pick::Rewind {
                conversation,
                checkpoint,
            } => self.rewind(conversation, *checkpoint).await?,
        };
        let Some(html) = done else {
            // A guest's list can be pressed by anyone in the chat. Their press leads to
            // a session of their own, which has none of the listed things.
            let gone = if query.message.is_none() {
                GUEST_GONE
            } else {
                GONE
            };
            return bot.answer_callback(&query.id, Some(gone)).await;
        };
        self.close_list(bot, query, &html).await
    }

    /// Turns a list into the record of what was picked from it.
    async fn close_list(&self, bot: &Bot, query: &CallbackQuery, html: &str) -> Result<()> {
        let _ = bot.answer_callback(&query.id, None).await;
        match (&query.message, &query.inline_message_id) {
            (Some(list), _) => {
                bot.edit_message(self.chat_id(), list.message_id, html, None)
                    .await
            }
            // A guest's list is the reply its summons got.
            (None, Some(reply)) => bot.edit_guest_reply(reply, html, None).await,
            (None, None) => bot.send_message(self.chat_id(), html, None).await.map(drop),
        }
    }

    /// `/effort`: sets the level named after the command, or lists them to pick from.
    /// Nothing has to be stopped for it: the level is handed over with each message, so
    /// it is the next turn that sees it.
    fn on_effort(&self, named: Option<&str>) -> (String, Option<Value>) {
        // A guest's list has no buttons: it names the levels to send instead.
        let picker = || {
            let effort = self.session.borrow().effort;
            if self.is_guest() {
                (render::effort_levels(effort), None)
            } else {
                let (html, keyboard) = render::effort_picker(effort);
                (html, Some(keyboard))
            }
        };
        let Some(word) = named else {
            return picker();
        };
        match render::parse_effort(word) {
            Some(effort) => {
                self.update(|session| session.effort = effort);
                (render::effort_set(effort), None)
            }
            None => {
                let (html, keyboard) = picker();
                let unknown = format!("I don't know the level {}.", render::escape(word));
                (format!("{unknown}\n\n{html}"), keyboard)
            }
        }
    }

    /// Makes an earlier conversation the one the chat is in. Returns what to say, or
    /// `None` if that conversation is not kept any more.
    async fn resume(&self, id: &str) -> Result<Option<String>> {
        let Some(conversation) = self.store.conversation(id)? else {
            return Ok(None);
        };
        let last = self.store.checkpoints(id)?.into_iter().next();
        if self.session.borrow().claude_session_id != id {
            self.switch(|| {
                self.store.clear_approvals()?;
                self.store.forget_all()?;
                // Used now, which also makes it the last to be dropped for its age.
                self.store.set_head(id, conversation.head, now_ms())?;
                self.update(|session| {
                    session.claude_session_id = id.to_owned();
                    session.has_state = true;
                    session.checkpoint = conversation.head;
                    session.needs_restore = false;
                    session.last_seq = 0;
                });
                Ok(())
            })
            .await?;
        }
        let last = last.as_ref().map(|last| last.text.as_str());
        Ok(Some(render::resumed(&conversation, last)))
    }

    /// Takes the conversation back to before the turn that `checkpoint` was saved after.
    /// Returns what to say, or `None` if the list the press came from is out of date.
    ///
    /// `picked` is the conversation as the button names it, which is by the start of its
    /// id; see [`render::conversation_tag`].
    async fn rewind(&self, picked: &str, checkpoint: u64) -> Result<Option<String>> {
        let (name, conversation) = {
            let session = self.session.borrow();
            let current = render::conversation_tag(&session.claude_session_id);
            if current != render::conversation_tag(picked) {
                return Ok(None);
            }
            (session.name.clone(), session.claude_session_id.clone())
        };
        let conversation = conversation.as_str();
        let kept = self.store.checkpoints(conversation)?;
        let Some((undone, Some(parent))) = render::rewindable(&kept)
            .into_iter()
            .find(|kept| kept.at_ms == checkpoint)
            .map(|kept| (kept.text.clone(), kept.parent))
        else {
            return Ok(None);
        };
        // That turn and every later one.
        let dropped: Vec<u64> = kept
            .iter()
            .map(|kept| kept.at_ms)
            .filter(|at_ms| *at_ms >= checkpoint)
            .collect();
        self.switch(|| {
            if parent == 0 {
                // Nothing of it is left to resume. It goes on under the same id.
                self.store.delete_conversation(conversation)?;
            } else {
                for at_ms in &dropped {
                    self.store.delete_checkpoint(conversation, *at_ms)?;
                }
                self.store.set_head(conversation, parent, now_ms())?;
            }
            self.update(|session| {
                session.has_state = parent != 0;
                session.checkpoint = parent;
                session.needs_restore = false;
                session.last_seq = 0;
            });
            Ok(())
        })
        .await?;
        // Checkpoints are named by when they were saved, so a later turn can never be
        // saved under one of these names while they are being deleted.
        let keys = dropped
            .into_iter()
            .map(|at_ms| checkpoint_key(&name, conversation, at_ms));
        self.discard(keys).await;
        Ok(Some(render::rewound(&undone)))
    }

    /// Drops what is kept beyond the limits: the conversation's oldest checkpoints, and
    /// the conversations that were not used for longest. Returns their archives' keys.
    fn prune(&self, name: &str, current: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for old in self
            .store
            .checkpoints(current)?
            .iter()
            .skip(CHECKPOINTS_KEPT)
        {
            self.store.delete_checkpoint(current, old.at_ms)?;
            keys.push(checkpoint_key(name, current, old.at_ms));
        }
        let others = self.store.conversations()?;
        let others = others.iter().filter(|other| other.id != current);
        for old in others.skip(CONVERSATIONS_KEPT - 1) {
            for checkpoint in self.store.checkpoints(&old.id)? {
                keys.push(checkpoint_key(name, &old.id, checkpoint.at_ms));
            }
            self.store.delete_conversation(&old.id)?;
        }
        Ok(keys)
    }

    /// `/stop`, and the Stop button of a draft.
    async fn stop(&self, cfg: &Config, bot: &Bot) -> Stopped {
        let Some(turn) = self.turn() else {
            return Stopped::Nothing;
        };
        if !turn.delivered {
            // The runner has not been given the message, so there is nothing to
            // interrupt: the turn simply ends here. A `drive` on its way to the runner
            // reads the turn again before sending and finds it gone.
            self.close_turn(cfg.idle_ttl_ms).await;
            self.close_status(bot, &turn, render::STOPPED).await;
            return Stopped::Turn;
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
        Stopped::Runner
    }

    /// The Stop button of a draft was pressed, which means what `/stop` means. The
    /// press is for the turn the draft was of: one that arrives when that turn is over
    /// says nothing about the next.
    async fn on_draft_stopped(
        &self,
        cfg: &Config,
        bot: &Bot,
        stopped: &StoppedGeneration,
    ) -> Result<()> {
        let pressed = stopped.draft_id;
        if !self
            .turn()
            .is_some_and(|turn| turn.draft_id != 0 && turn.draft_id == pressed)
        {
            return Ok(());
        }
        // Telegram took the draft away with the press.
        self.live.borrow_mut().draft_shown = false;
        match self.stop(cfg, bot).await {
            // Nothing else will say that this turn is over.
            ended @ Stopped::Turn => bot
                .send_message(self.chat_id(), ended.said(), None)
                .await
                .map(drop),
            // The button gave way under the press, and the end of the turn says the rest.
            Stopped::Runner | Stopped::Nothing => Ok(()),
        }
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
        let effort = self.session.borrow().effort;
        lines.push(format!("Effort: {}.", render::effort_name(effort)));
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
        // One started by hand is up already.
        if self.cfg().is_ok_and(|cfg| cfg.dev_mcp_url.is_some()) {
            return;
        }
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
            effort: self.session.borrow().effort,
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
                let closed = self
                    .close_approvals(&Bot::new(cfg), false, "The agent was restarted.")
                    .await;
                if closed {
                    self.redraw_status().await;
                }
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
            // Not a numbered frame: it is shown or it is not, and nothing is kept of it.
            FromRunner::Partial { turn_id, text } => return self.show_draft(turn_id, text).await,
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
                    // A tool call: what Claude wrote before it was not the answer, and
                    // is part of the progress now.
                    self.clear_draft().await;
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
        let now = now_ms();
        {
            let mut live = self.live.borrow_mut();
            if now.saturating_sub(live.last_status_edit_ms) < STATUS_EDIT_INTERVAL_MS {
                return;
            }
            live.last_status_edit_ms = now;
        }
        self.redraw_status().await;
    }

    /// Notes that a message now sits below the turn's progress message.
    fn bury_status(&self) {
        if self.turn().is_some_and(|turn| !turn.status_buried) {
            self.update(|session| {
                if let Some(turn) = &mut session.turn {
                    turn.status_buried = true;
                }
            });
        }
    }

    /// Shows the turn's progress as it stands. A progress message that prompts, or the
    /// user's own messages, have pushed up the chat is replaced by a new one below them,
    /// so that the newest message is always the one that is changing. That waits while a
    /// prompt is open: until it is answered, the prompt is what should be read first.
    async fn redraw_status(&self) {
        let Some(turn) = self.turn() else { return };
        let Ok(cfg) = self.cfg() else { return };
        let bot = Bot::new(&cfg);
        if self.is_guest() {
            return self.redraw_guest_reply(&bot, &turn).await;
        }
        let Some(message_id) = turn.status_message_id else {
            return;
        };
        let chat_id = self.chat_id();
        let html = self.live.borrow().status.render(render::WORKING);
        let prompt_is_open = self.store.approvals().is_ok_and(|open| !open.is_empty());
        if !turn.status_buried || prompt_is_open {
            let _ = bot.edit_message(chat_id, message_id, &html, None).await;
            return;
        }

        // Cleared before the first await, so that two events cannot both move it.
        let set_buried = |buried| {
            self.update(|session| {
                if let Some(open) = &mut session.turn {
                    open.status_buried = buried;
                }
            });
        };
        set_buried(false);
        let Ok(moved) = bot.send_message(chat_id, &html, None).await else {
            return set_buried(true);
        };
        // The turn may have been closed while that was on its way. Its progress message
        // then has its last form already, and the new one is the one to remove.
        let adopted = self.update(|session| match &mut session.turn {
            Some(open) if open.id == turn.id && open.status_message_id == Some(message_id) => {
                open.status_message_id = Some(moved);
                true
            }
            _ => false,
        });
        let left_over = if adopted { message_id } else { moved };
        let _ = bot.delete_message(chat_id, left_over).await;
    }

    /// Draws a guest's reply as its turn stands: the prompt that waits for an answer, if
    /// one does, and the progress otherwise. One message has to do for both, so a prompt
    /// takes the place of the progress until it is answered.
    async fn redraw_guest_reply(&self, bot: &Bot, turn: &Turn) {
        let Some(reply) = &turn.guest_reply else {
            return;
        };
        let (html, keyboard) = match self.guest_prompt() {
            Some((html, keyboard)) => (html, Some(keyboard)),
            // Progress can wait while Telegram asks for fewer calls. A prompt cannot:
            // the turn stands still until it is answered.
            None if now_ms() < self.live.borrow().quiet_until_ms => return,
            None => (self.live.borrow().status.render(render::WORKING), None),
        };
        if let Err(error) = bot.edit_guest_reply(reply, &html, keyboard.as_ref()).await {
            self.keep_quiet(&error);
        }
    }

    /// Notes for how long Telegram asked to be left alone, if that is why a call failed.
    /// Returns the seconds.
    fn keep_quiet(&self, error: &worker::Error) -> Option<u64> {
        let seconds = update::retry_after(&error.to_string())?;
        self.live.borrow_mut().quiet_until_ms = now_ms() + seconds * 1000;
        Some(seconds)
    }

    /// The prompt a guest's reply is to show: of those that wait, the one that was asked
    /// first, at the question it has got to.
    fn guest_prompt(&self) -> Option<(String, Value)> {
        let approval = self.store.approvals().ok()?.into_iter().next()?;
        let (html, keyboard) = if approval.tool_name == ASK_USER_QUESTION {
            let next = approval.answers.len();
            render::question_prompt(&approval.request_id, &approval.input, next)?
        } else {
            render::permission_prompt(&approval.request_id, &approval.tool_name, &approval.input)
        };
        Some((html, render::for_guest(keyboard, self.chat_id())))
    }

    /// Gives a guest's reply its last form, which a turn that is over has to get: there
    /// is no other message to say how it went. Nothing touches the reply after this.
    async fn close_guest_reply(&self, bot: &Bot, turn: &Turn, html: &str) {
        // Read from the open turn where there still is one, as in `close_status`.
        let reply = self.update(|session| match &mut session.turn {
            Some(open) if open.id == turn.id => open.guest_reply.take(),
            _ => turn.guest_reply.clone(),
        });
        let Some(reply) = reply else { return };
        // Kept first and sent from what is kept. Many guests' replies in one chat are
        // many edits there, and when Telegram refuses this one for coming too soon, the
        // answer must not be lost with the attempt.
        let now = now_ms();
        self.update(|session| {
            session.unsent.push(Unsent {
                reply,
                html: html.to_owned(),
                since_ms: now,
                retry_at_ms: now,
                tries: 0,
            });
        });
        self.send_unsent(bot).await;
    }

    /// Offers Telegram the replies it has still to take, those whose time has come, and
    /// sets the alarm for what it does not take. A refusal for too many calls says how
    /// long to wait, and that is how long this waits, however long it is; anything else
    /// is tried a few times. Nothing is kept past [`UNSENT_KEPT_MS`].
    async fn send_unsent(&self, bot: &Bot) {
        let now = now_ms();
        let due: Vec<Unsent> = {
            let session = self.session.borrow();
            let due = session
                .unsent
                .iter()
                .filter(|unsent| unsent.retry_at_ms <= now);
            due.cloned().collect()
        };
        for unsent in due {
            let refused = bot
                .edit_guest_reply(&unsent.reply, &unsent.html, None)
                .await
                .err();
            let now = now_ms();
            let again = refused.and_then(|error| {
                let wait = self.keep_quiet(&error);
                let tries = unsent.tries + 1;
                let old = now.saturating_sub(unsent.since_ms) > UNSENT_KEPT_MS;
                if old || (wait.is_none() && tries >= UNSENT_TRIES) {
                    console_error!("a guest's reply was never finished: {error}");
                    return None;
                }
                console_warn!("a guest's reply is not finished yet: {error}");
                // A second more than asked: the clocks need not agree.
                let pause = wait.map_or(UNSENT_RETRY_MS, |seconds| (seconds + 1) * 1000);
                Some((now + pause, tries))
            });
            self.update(|session| match again {
                Some((retry_at_ms, tries)) => {
                    let kept = session.unsent.iter_mut();
                    for kept in kept.filter(|kept| kept.reply == unsent.reply) {
                        kept.retry_at_ms = retry_at_ms;
                        kept.tries = tries;
                    }
                }
                None => session.unsent.retain(|kept| kept.reply != unsent.reply),
            });
        }
        let next = self
            .session
            .borrow()
            .unsent
            .iter()
            .map(|unsent| unsent.retry_at_ms)
            .min();
        if let Some(at_ms) = next {
            self.arm(at_ms.saturating_sub(now_ms())).await;
        }
    }

    // ----- The text being written -----

    /// Shows what Claude is writing as a draft, the message Telegram draws as still
    /// being typed. It is a preview only: the answer is sent when the turn is over, as
    /// it is without drafts, and the draft goes when a message arrives.
    async fn show_draft(&self, turn_id: &str, text: &str) {
        let Some(turn) = self
            .turn()
            .filter(|turn| turn.id == turn_id && turn.draft_id != 0 && !turn.stop_requested)
        else {
            return;
        };
        let now = now_ms();
        {
            let mut live = self.live.borrow_mut();
            if live.drafts_off || now.saturating_sub(live.last_draft_ms) < DRAFT_INTERVAL_MS {
                return;
            }
            live.last_draft_ms = now;
        }
        if let Some(html) = render::draft(text) {
            self.send_draft(&turn, &html).await;
        }
    }

    /// Takes the text off the draft once it turned out not to be the answer. What is
    /// left is Telegram's placeholder for a message that is on its way.
    async fn clear_draft(&self) {
        if !self.live.borrow().draft_shown {
            return;
        }
        if let Some(turn) = self.turn() {
            self.send_draft(&turn, "").await;
        }
    }

    async fn send_draft(&self, turn: &Turn, html: &str) {
        let Ok(cfg) = self.cfg() else { return };
        let sent = Bot::new(&cfg)
            .send_draft(self.chat_id(), turn.draft_id, html)
            .await;
        let mut live = self.live.borrow_mut();
        match sent {
            Ok(true) => live.draft_shown = !html.is_empty(),
            Ok(false) => {}
            Err(error) => {
                // An older Bot API server, for one, has no drafts. The turn loses
                // nothing but the preview.
                console_warn!("no drafts for this turn: {error}");
                live.drafts_off = true;
            }
        }
    }

    /// Gives the progress message its last form when a turn is over. It stays in the chat
    /// as the record of what was done; one that never got past "Working…" is removed.
    /// Nothing touches the message after this.
    ///
    /// A guest's turn has no progress message: its reply gets the heading, until the
    /// answer takes its place.
    async fn close_status(&self, bot: &Bot, turn: &Turn, heading: &str) {
        if self.is_guest() {
            let html = self.live.borrow().status.render(heading);
            return self.close_guest_reply(bot, turn, &html).await;
        }
        // Read from the open turn where there still is one: `turn` may be a copy made
        // before the message was last moved.
        let message_id = self.update(|session| match &mut session.turn {
            Some(open) if open.id == turn.id => open.status_message_id.take(),
            _ => turn.status_message_id,
        });
        let Some(message_id) = message_id else {
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
        let guest = self.is_guest();
        // An answer covers all of a request, so one too long for its prompt goes ahead
        // of it as a file. If that cannot be sent the user is not asked, and the request
        // is declined like any other that could not be shown.
        if tool_name != ASK_USER_QUESTION
            && let Some(whole) = render::permission_overflow(&tool_name, &input)
        {
            if guest {
                // The file would have to be where the prompt is, and a guest can send
                // none there.
                return Err(error("the request is too long to show as a guest"));
            }
            let file = whole.as_bytes();
            bot.send_document(self.chat_id(), render::REQUEST_FILE, file, None)
                .await?;
        }
        let mut approval = Approval {
            request_id,
            turn_id,
            tool_name,
            input,
            message_id: 0,
            deadline_ms: now_ms() + cfg.approval_timeout_ms,
            answers: Default::default(),
        };
        if guest {
            // The prompt goes into the reply, in place of the progress. Behind an
            // earlier one it waits there until that is answered, see `guest_prompt`.
            let reply = self
                .turn()
                .and_then(|turn| turn.guest_reply)
                .ok_or_else(|| error("the reply to ask in is gone"))?;
            let first = self.store.approvals()?.is_empty();
            self.store.put_approval(&approval)?;
            if first {
                let keyboard = render::for_guest(keyboard, self.chat_id());
                let shown = bot.edit_guest_reply(&reply, &html, Some(&keyboard)).await;
                if shown.is_err() {
                    self.store.delete_approval(&approval.request_id)?;
                }
                shown?;
            }
            return Ok(());
        }
        approval.message_id = bot
            .send_message(self.chat_id(), &html, Some(&keyboard))
            .await?;
        self.bury_status();
        self.store.put_approval(&approval)
    }

    async fn on_callback(
        self: &Rc<Self>,
        cfg: &Config,
        bot: &Bot,
        query: CallbackQuery,
    ) -> Result<()> {
        // A button on a guest's reply names its chat ahead of what it stands for.
        let data = query
            .data
            .as_deref()
            .map(|data| render::guest_chat(data).map_or(data, |(_, rest)| rest));
        if let Some(pick) = data.and_then(Pick::parse) {
            return self.on_pick(bot, &query, pick).await;
        }
        if let Some(effort) = data.and_then(render::picked_effort) {
            self.update(|session| session.effort = effort);
            return self
                .close_list(bot, &query, &render::effort_set(effort))
                .await;
        }
        let callback = data.and_then(Callback::parse);
        let approval = match &callback {
            Some(callback) => self.store.approval(callback.request_id())?,
            None => None,
        };
        // A guest's session is that of the user it answers, so nobody else's press gets
        // here. It gets to a session of their own, which has no such prompt.
        let guest = self.is_guest();
        let (Some(callback), Some(mut approval)) = (callback, approval) else {
            let gone = if query.message.is_none() {
                GUEST_GONE
            } else {
                GONE
            };
            return bot.answer_callback(&query.id, Some(gone)).await;
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
                    if guest {
                        self.redraw_status().await;
                        return Ok(());
                    }
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
        // A guest's prompt was shown in the reply, which the progress takes back below.
        let edited = if guest {
            Ok(())
        } else {
            let (prompt, _) = self.prompt_of(&approval);
            let html = format!("{prompt}\n\n{outcome}");
            bot.edit_message(chat_id, approval.message_id, &html, None)
                .await
        };
        // With the prompt answered, the progress is what to watch again.
        self.redraw_status().await;
        edited
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
    /// Returns whether there was one to close.
    async fn close_approvals(&self, bot: &Bot, only_expired: bool, reason: &str) -> bool {
        let now = now_ms();
        let Ok(approvals) = self.store.approvals() else {
            return false;
        };
        let guest = self.is_guest();
        let mut closed = false;
        for approval in approvals {
            if only_expired && approval.deadline_ms > now {
                continue;
            }
            closed = true;
            self.send(&ToRunner::PermissionDecision {
                request_id: approval.request_id.clone(),
                decision: Decision::Deny {
                    message: reason.to_owned(),
                },
            });
            let _ = self.store.delete_approval(&approval.request_id);
            if guest {
                // It has no message of its own to mark: the reply it was shown in goes
                // on to whatever the turn has to show next.
                continue;
            }
            let (prompt, _) = self.prompt_of(&approval);
            let html = format!("{prompt}\n\n<b>⌛ Expired</b>");
            let _ = bot
                .edit_message(self.chat_id(), approval.message_id, &html, None)
                .await;
        }
        closed
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
        if self.is_guest() {
            return self.report_as_guest(&bot, turn, result, &messages).await;
        }
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
            let told = match self.deliver_artifact(&bot, turn, artifact).await {
                Ok(filed) => match kept_note(artifact, &filed) {
                    Some(note) => bot
                        .send_message(self.chat_id(), &note, None)
                        .await
                        .map(drop),
                    None => Ok(()),
                },
                Err(error) => Err(error),
            };
            if let Err(error) = told {
                console_error!("delivering {} failed: {error}", artifact.path);
                problems.push(format!(
                    "I could not send {}.",
                    render::escape(&artifact.path)
                ));
            }
        }
        if let Err(error) = self.save_state(turn).await {
            console_error!("saving session state failed: {error}");
            problems.push("I could not save this conversation; it may be forgotten.".to_owned());
        }
        if !problems.is_empty() {
            let _ = self
                .deliver(&bot, &format!("⚠️ {}", problems.join("\n")))
                .await;
        }
    }

    /// What [`Self::report_turn`] owes a guest, whose one message has to say all of it:
    /// the answer, as much of it as fits, and under it what became of the files and what
    /// went wrong. So the message is written last, when all of that is known.
    async fn report_as_guest(
        &self,
        bot: &Bot,
        turn: &Turn,
        result: &TurnResult,
        messages: &[String],
    ) {
        self.close_approvals(bot, false, "The turn is over.").await;
        let mut notes = Vec::new();
        for artifact in &result.artifacts {
            let filed = self.deliver_artifact(bot, turn, artifact).await;
            if let Err(error) = &filed {
                console_error!("delivering {} failed: {error}", artifact.path);
            }
            notes.push(guest_file_note(artifact, filed.as_ref().ok()));
        }
        if let Err(error) = self.save_state(turn).await {
            console_error!("saving session state failed: {error}");
            notes.push("⚠️ I could not save this conversation; it may be forgotten.".to_owned());
        }
        let html = render::one_message(messages, &notes);
        self.close_guest_reply(bot, turn, &html).await;
    }

    /// Gives up on a turn the runner will never finish.
    async fn abandon_turn(&self, message: &str) {
        let turn = self.update(|session| session.turn.take());
        if let Ok(cfg) = self.cfg() {
            let bot = Bot::new(&cfg);
            let guest = self.is_guest();
            if let Some(turn) = &turn
                && !guest
            {
                self.close_status(&bot, turn, render::FAILED).await;
            }
            self.close_approvals(&bot, false, "The turn was abandoned.")
                .await;
            match &turn {
                // Why it ended is all the reply has left to say.
                Some(turn) if guest => self.close_guest_reply(&bot, turn, message).await,
                _ if guest => {}
                _ => {
                    let _ = self.deliver(&bot, message).await;
                }
            }
        }
        // Whatever container is left is stopped by the idle branch of the alarm.
        self.update(|session| session.idle_deadline_ms = now_ms());
        self.arm(RETRY_MS).await;
    }

    /// Copies an output file to R2 and, if it is small enough, into the chat: for a
    /// guest, the private chat of the user it answers, see [`Self::files_chat`]. What
    /// is said about a file that was not sent is up to the caller, which knows where
    /// that can be said.
    async fn deliver_artifact(&self, bot: &Bot, turn: &Turn, artifact: &Artifact) -> Result<Filed> {
        let mut url = Url::parse("http://runner/files/out")?;
        url.path_segments_mut()
            .map_err(|()| error("bad artifact URL"))?
            .extend(artifact.path.split('/'));
        let response = self.runner(Method::Get, url.path(), None).await?;
        let mut response = expect_ok(response, "reading an output file").await?;

        let bucket = self.env.bucket(STATE_BUCKET)?;
        let name = self.session.borrow().name.clone();
        let key = format!("artifacts/{name}/{}/{}", turn.id, artifact.path);
        let file_name = file_name(artifact);
        let chat_id = self.files_chat();
        // In that chat nothing else says where a file suddenly comes from.
        let caption = self
            .is_guest()
            .then_some("From what you asked me in another chat.");

        if artifact.size > TELEGRAM_FILE_LIMIT {
            let stream = FixedLengthStream::wrap(response.stream()?, artifact.size);
            bucket.put(key.clone(), stream).execute().await?;
            return Ok(Filed::TooLarge(key));
        }

        // Stored first: the container's copy is gone once it stops, so a file Telegram
        // will not take must still exist somewhere.
        let bytes = response.bytes().await?;
        let stored = bucket.put(key.clone(), bytes.clone()).execute().await;
        let sent = bot.send_document(chat_id, file_name, &bytes, caption).await;
        match (sent, stored) {
            (Ok(()), Ok(_)) => Ok(Filed::Sent),
            (Ok(()), Err(error)) => {
                // The user has the file; only the archive copy is missing.
                console_error!("storing {key} failed: {error}");
                Ok(Filed::Sent)
            }
            (Err(error), Ok(_)) => {
                console_error!("sending {file_name} failed: {error}");
                Ok(Filed::Refused(key))
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    /// Stores the runner's state archive as a new checkpoint of the conversation: what
    /// the next container picks up from, and what `/rewind` and `/resume` go back to.
    /// Earlier checkpoints are left as they are, so a save that fails loses nothing.
    async fn save_state(&self, turn: &Turn) -> Result<()> {
        let (name, conversation, parent) = {
            let session = self.session.borrow();
            if session.needs_restore {
                return Err(error(
                    "this container never received the saved state; keeping the saved copy",
                ));
            }
            // What the turn started from. A state saved before there were checkpoints
            // is replaced by this one, so there is no going back to it.
            let parent =
                (!session.has_state || session.checkpoint != 0).then_some(session.checkpoint);
            (
                session.name.clone(),
                session.claude_session_id.clone(),
                parent,
            )
        };
        let response = self.runner(Method::Get, "/state", None).await?;
        let mut response = expect_ok(response, "exporting session state").await?;
        let length = response
            .headers()
            .get("content-length")?
            .and_then(|length| length.parse().ok())
            .ok_or_else(|| error("the state archive has no length"))?;
        let stream = FixedLengthStream::wrap(response.stream()?, length);
        let bucket: Bucket = self.env.bucket(STATE_BUCKET)?;
        // Later than what it follows even if the clock stood still or went back.
        let at_ms = now_ms().max(parent.unwrap_or(0) + 1);
        let key = checkpoint_key(&name, &conversation, at_ms);
        bucket.put(key, stream).execute().await?;
        let checkpoint = Checkpoint {
            at_ms,
            parent,
            text: turn.text.clone(),
        };
        self.store.put_checkpoint(&conversation, &checkpoint)?;
        self.update(|session| {
            session.has_state = true;
            session.checkpoint = at_ms;
        });

        // The conversation is saved; tidying up after it is not worth failing over.
        let mut unkept = self.prune(&name, &conversation).unwrap_or_else(|error| {
            console_error!("dropping old checkpoints failed: {error}");
            Vec::new()
        });
        if parent.is_none() {
            unkept.push(legacy_state_key(&name));
        }
        self.discard(unkept).await;
        Ok(())
    }

    // ----- The alarm -----

    /// An error returned from here makes [`SessionDO::alarm`] set the alarm again, so
    /// every early return is a retry, not an end.
    async fn on_alarm(self: &Rc<Self>) -> Result<()> {
        let cfg = self.cfg()?;
        let bot = Bot::new(&cfg);
        // Before anything else: this may be all the alarm was set for. It sets it again
        // for what is left, and no later setting below pushes that back.
        if !self.session.borrow().unsent.is_empty() {
            self.send_unsent(&bot).await;
        }
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
            // A guest has no message to say it in. Its reply says how the turn ended.
            if !self.is_guest() {
                let late = "⏱ This is taking too long; stopping.";
                let _ = bot.send_message(self.chat_id(), late, None).await;
            }
        }
        let expired = self
            .close_approvals(&bot, true, "The user did not answer in time.")
            .await;
        if expired {
            // A prompt nobody answered gives way to the progress again.
            self.redraw_status().await;
        }

        self.warm_mcp();
        self.arm(WATCHDOG_MS).await;
        self.drive().await;
        Ok(())
    }
}
