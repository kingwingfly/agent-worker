//! The session actor: the single owner of the `claude` process, the turn in progress,
//! pending permission prompts and the replay buffer.
//!
//! Everything reaches it as a message, from the WebSocket, from the approval tool and
//! from the process reader, so there is no shared mutable state to lock.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use agent_proto::{Decision, FromRunner, ToRunner, TurnResult};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    claude::{self, ChildEvent, Claude},
    config::Config,
    state::{self, Listing},
};

/// Frames kept for replay while the Durable Object has not acknowledged them. Past this
/// the oldest are dropped, which costs a reconnecting client the start of a long turn's
/// event stream but never its result.
const MAX_UNACKED_FRAMES: usize = 20_000;

enum Command {
    Attach(mpsc::UnboundedSender<FromRunner>),
    Frame(ToRunner),
    Permission {
        tool_name: String,
        input: Value,
        reply: oneshot::Sender<Decision>,
    },
    Started(oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<Command>,
}

impl SessionHandle {
    /// Registers a new control connection, replacing any previous one, and returns the
    /// frames to write to it. The first is always [`FromRunner::Hello`].
    pub fn attach(&self) -> mpsc::UnboundedReceiver<FromRunner> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.commands.send(Command::Attach(tx));
        rx
    }

    pub fn handle(&self, frame: ToRunner) {
        let _ = self.commands.send(Command::Frame(frame));
    }

    /// Puts a tool call to the user and waits for the answer.
    pub async fn request_permission(&self, tool_name: String, input: Value) -> Decision {
        let (reply, answer) = oneshot::channel();
        let _ = self.commands.send(Command::Permission {
            tool_name,
            input,
            reply,
        });
        answer.await.unwrap_or_else(|_| Decision::Deny {
            message: "The approval request was dropped.".into(),
        })
    }

    /// Whether `claude` has been launched at least once. State can only be restored
    /// before that.
    pub async fn started(&self) -> bool {
        let (reply, answer) = oneshot::channel();
        let _ = self.commands.send(Command::Started(reply));
        answer.await.unwrap_or(true)
    }
}

struct Turn {
    id: String,
    text: String,
    /// Identifies the user message in Claude's stream.
    message_uuid: String,
    /// Claude has picked the message up, so the next `result` is its answer.
    acknowledged: bool,
    /// The message was already sent to a second process after the first one died on it.
    resent: bool,
    /// Output directory listing when the turn began.
    before: Listing,
    interrupted: bool,
}

struct Session {
    cfg: Arc<Config>,
    /// `--mcp-config` JSON for every Claude this session launches.
    mcp_config: String,
    child_events: mpsc::UnboundedSender<ChildEvent>,
    claude: Option<Claude>,
    /// Bumped on every launch; events from an older process are ignored.
    generation: u64,
    started: bool,
    seq: u64,
    unacked: VecDeque<FromRunner>,
    connection: Option<mpsc::UnboundedSender<FromRunner>>,
    turn: Option<Turn>,
    last_turn: Option<String>,
    /// Prompts waiting for the user, by request id. Whether an answer is remembered for
    /// later requests is the Durable Object's business: it outlives this process.
    pending: HashMap<String, oneshot::Sender<Decision>>,
}

pub fn spawn(cfg: Arc<Config>, mcp_config: String) -> SessionHandle {
    let (commands_tx, mut commands) = mpsc::unbounded_channel();
    let (child_events, mut child_rx) = mpsc::unbounded_channel();
    let mut session = Session {
        cfg,
        mcp_config,
        child_events,
        claude: None,
        generation: 0,
        started: false,
        seq: 0,
        unacked: VecDeque::new(),
        connection: None,
        turn: None,
        last_turn: None,
        pending: HashMap::new(),
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => session.on_command(command).await,
                    None => break,
                },
                // The session holds a sender, so this never yields `None`.
                Some(event) = child_rx.recv() => session.on_child_event(event).await,
            }
        }
    });
    SessionHandle {
        commands: commands_tx,
    }
}

fn turn_result(result: &Value) -> TurnResult {
    TurnResult {
        is_error: result["is_error"].as_bool().unwrap_or(false),
        subtype: result["subtype"].as_str().unwrap_or("unknown").to_owned(),
        text: result["result"].as_str().map(str::to_owned),
        usage: serde_json::from_value(result["usage"].clone()).unwrap_or_default(),
        artifacts: Vec::new(),
    }
}

fn failure(subtype: &str, message: impl Into<String>) -> TurnResult {
    TurnResult {
        is_error: true,
        subtype: subtype.to_owned(),
        text: Some(message.into()),
        ..TurnResult::default()
    }
}

impl Session {
    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Attach(connection) => {
                let _ = connection.send(FromRunner::Hello {
                    runner_version: env!("CARGO_PKG_VERSION").to_owned(),
                    last_seq: self.seq,
                    running_turn: self.turn.as_ref().map(|turn| turn.id.clone()),
                });
                self.connection = Some(connection);
            }
            Command::Frame(ToRunner::UserMessage { turn_id, text }) => {
                self.start_turn(turn_id, text).await
            }
            Command::Frame(ToRunner::PermissionDecision {
                request_id,
                decision,
            }) => self.resolve_permission(&request_id, decision),
            Command::Frame(ToRunner::Interrupt) => self.interrupt(),
            Command::Frame(ToRunner::Ack { seq }) => {
                while self
                    .unacked
                    .front()
                    .is_some_and(|frame| frame.seq() <= Some(seq))
                {
                    self.unacked.pop_front();
                }
            }
            Command::Frame(ToRunner::Resume { after_seq }) => {
                let replay: Vec<_> = self
                    .unacked
                    .iter()
                    .filter(|frame| frame.seq() > Some(after_seq))
                    .cloned()
                    .collect();
                replay.into_iter().for_each(|frame| self.send(frame));
            }
            Command::Permission {
                tool_name,
                input,
                reply,
            } => self.request_permission(tool_name, input, reply),
            Command::Started(reply) => {
                let _ = reply.send(self.started);
            }
        }
    }

    async fn on_child_event(&mut self, event: ChildEvent) {
        match event {
            ChildEvent::Line { generation, line } if generation == self.generation => {
                let Some(turn) = &mut self.turn else {
                    tracing::debug!("dropping claude output outside a turn");
                    return;
                };
                turn.acknowledged |= claude::is_replay_of(&line, &turn.message_uuid);
                let turn_id = turn.id.clone();
                let result =
                    (turn.acknowledged && line["type"] == "result").then(|| turn_result(&line));
                self.emit(|seq| FromRunner::Event {
                    seq,
                    turn_id,
                    event: line,
                });
                if let Some(result) = result {
                    self.finish_turn(result).await;
                }
            }
            ChildEvent::Exited { generation, detail } if generation == self.generation => {
                tracing::info!("{detail}");
                self.claude = None;
                match &mut self.turn {
                    // Checked first: a turn the user stopped is over, even if Claude had
                    // not picked its message up yet. Sending it again would run exactly
                    // what was cancelled.
                    Some(turn) if turn.interrupted => {
                        let result = TurnResult {
                            subtype: "interrupted".into(),
                            ..TurnResult::default()
                        };
                        self.finish_turn(result).await;
                    }
                    // Claude exits shortly after an interrupted turn, and the runner only
                    // learns of an exit from this event, so the next message can land in
                    // a process that is already on its way out. A message that was never
                    // acknowledged was never acted on, which makes sending it again safe.
                    Some(turn) if !turn.acknowledged && !turn.resent => {
                        turn.resent = true;
                        self.deliver().await;
                    }
                    Some(_) => self.finish_turn(failure("runner_error", detail)).await,
                    None => {}
                }
            }
            ChildEvent::Line { .. } | ChildEvent::Exited { .. } => {}
        }
    }

    /// Assigns the next sequence number, sends the frame and keeps it for replay.
    fn emit(&mut self, build: impl FnOnce(u64) -> FromRunner) {
        self.seq += 1;
        let frame = build(self.seq);
        self.send(frame.clone());
        if self.unacked.len() == MAX_UNACKED_FRAMES {
            self.unacked.pop_front();
        }
        self.unacked.push_back(frame);
    }

    fn send(&mut self, frame: FromRunner) {
        if let Some(connection) = &self.connection
            && connection.send(frame).is_err()
        {
            self.connection = None;
        }
    }

    async fn scan_output(&self) -> Listing {
        let out_dir = self.cfg.out_dir.clone();
        tokio::task::spawn_blocking(move || state::scan(&out_dir))
            .await
            .unwrap_or_default()
    }

    async fn start_turn(&mut self, turn_id: String, text: String) {
        let running = self.turn.as_ref().map(|turn| turn.id.as_str());
        // The Durable Object re-sends a message it is not sure was delivered.
        if running == Some(turn_id.as_str()) || self.last_turn.as_deref() == Some(turn_id.as_str())
        {
            return;
        }
        if running.is_some() {
            let result = failure("runner_busy", "Another turn is still running.");
            self.emit(|seq| FromRunner::TurnDone {
                seq,
                turn_id,
                result,
            });
            return;
        }

        let before = self.scan_output().await;
        self.turn = Some(Turn {
            id: turn_id,
            text,
            message_uuid: uuid::Uuid::new_v4().to_string(),
            acknowledged: false,
            resent: false,
            before,
            interrupted: false,
        });
        self.deliver().await;
    }

    /// Hands the current turn's message to Claude, launching it first if necessary.
    async fn deliver(&mut self) {
        let Some(turn) = &self.turn else { return };
        let (message_uuid, text) = (turn.message_uuid.clone(), turn.text.clone());

        if self.claude.is_none() {
            self.generation += 1;
            let resume = self.cfg.has_transcript();
            match Claude::spawn(
                &self.cfg,
                &self.mcp_config,
                resume,
                self.generation,
                self.child_events.clone(),
            ) {
                Ok(claude) => {
                    self.claude = Some(claude);
                    self.started = true;
                }
                Err(error) => {
                    self.finish_turn(failure("runner_error", format!("{error:#}")))
                        .await;
                    return;
                }
            }
        }
        let claude = self.claude.as_mut().expect("claude was just started");
        if let Err(error) = claude.send_user_message(&message_uuid, &text).await {
            // The process is gone or going. Make sure of it; its exit event then decides
            // whether the message is sent again or the turn fails.
            tracing::warn!(%error, "could not write to claude");
            claude.kill();
            self.claude = None;
        }
    }

    async fn finish_turn(&mut self, mut result: TurnResult) {
        let Some(turn) = self.turn.take() else { return };
        result.artifacts = state::changed(&turn.before, &self.scan_output().await);
        self.deny_pending("The turn ended before this was answered.");
        self.last_turn = Some(turn.id.clone());
        self.emit(|seq| FromRunner::TurnDone {
            seq,
            turn_id: turn.id,
            result,
        });
    }

    fn interrupt(&mut self) {
        let Some(turn) = &mut self.turn else { return };
        turn.interrupted = true;
        if let Some(claude) = &self.claude {
            claude.interrupt();
        }
        self.deny_pending("Interrupted by the user.");
    }

    fn request_permission(
        &mut self,
        tool_name: String,
        input: Value,
        reply: oneshot::Sender<Decision>,
    ) {
        let Some(turn) = &self.turn else {
            let _ = reply.send(Decision::Deny {
                message: "No turn is running.".into(),
            });
            return;
        };
        let turn_id = turn.id.clone();
        let request_id = uuid::Uuid::new_v4().to_string();
        self.pending.insert(request_id.clone(), reply);
        self.emit(|seq| FromRunner::PermissionRequest {
            seq,
            turn_id,
            request_id,
            tool_name,
            input,
        });
    }

    fn resolve_permission(&mut self, request_id: &str, decision: Decision) {
        if let Some(reply) = self.pending.remove(request_id) {
            let _ = reply.send(decision);
        }
    }

    fn deny_pending(&mut self, message: &str) {
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Decision::Deny {
                message: message.to_owned(),
            });
        }
    }
}
