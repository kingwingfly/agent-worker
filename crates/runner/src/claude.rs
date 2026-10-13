//! The `claude` child process: how it is launched and how its stream-json output is read.

use std::{collections::VecDeque, path::PathBuf, process::Stdio};

use agent_proto::Effort;
use anyhow::Context;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
};

use crate::config::Config;

/// MCP server name the runner registers its approval tool under.
pub const APPROVER_SERVER: &str = "approver";
pub const APPROVER_TOOL: &str = "permission_prompt";
/// MCP server name of the shared domain server behind the Worker.
pub const DOMAIN_SERVER: &str = "domain";

/// How long Claude waits on one MCP tool call. A permission prompt stays open until the
/// user answers, so this has to outlast any approval timeout the Durable Object applies.
const MCP_TOOL_TIMEOUT_MS: &str = "3600000";
const STDERR_TAIL_LINES: usize = 20;

/// What the reader task reports back. `generation` tells a restarted process apart from
/// the one it replaced.
#[derive(Debug)]
pub enum ChildEvent {
    Line { generation: u64, line: Value },
    Exited { generation: u64, detail: String },
}

pub struct Claude {
    stdin: ChildStdin,
    pid: Option<u32>,
    /// The level it answers at: the one it was launched with, until it is told another.
    effort: Option<Effort>,
}

/// The MCP servers Claude connects to, as `--mcp-config` takes them.
///
/// Passed on the command line, not through a file: the agent runs as the same user and
/// could rewrite a file to point "approver" at a server of its own before Claude is next
/// launched, and so approve its own requests.
pub fn mcp_config(cfg: &Config, approver_url: &str) -> Value {
    let mut servers = serde_json::Map::new();
    servers.insert(
        APPROVER_SERVER.into(),
        json!({ "type": "http", "url": approver_url }),
    );
    if let Some(upstream) = &cfg.upstream {
        servers.insert(
            DOMAIN_SERVER.into(),
            json!({
                "type": "http",
                "url": format!("{}/mcp", upstream.origin),
                "headers": { "Authorization": format!("Bearer {}", upstream.token) },
            }),
        );
    }
    json!({ "mcpServers": servers })
}

/// Arguments for a long-lived, stream-json driven `claude -p`. `mcp_config` is the JSON
/// from [`mcp_config`]. Without an `effort` the CLI picks its default for the model.
pub fn args(cfg: &Config, mcp_config: &str, resume: bool, effort: Option<Effort>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        // The model's response piece by piece as well, on `stream_event` lines. The text
        // being written is shown from those; they are not passed on one by one, see
        // `piece`.
        "--include-partial-messages",
        // Echoes each user message back when Claude picks it up; see `is_replay_of`.
        "--replay-user-messages",
        // Edits inside the working directory need no prompt; everything not covered by
        // the allow-list in the settings file goes to the approval tool.
        "--permission-mode",
        "acceptEdits",
        "--permission-prompt-tool",
    ]
    .map(String::from)
    .into();
    args.push(format!("mcp__{APPROVER_SERVER}__{APPROVER_TOOL}"));
    args.extend([
        "--tools".into(),
        cfg.tools.clone(),
        "--mcp-config".into(),
        mcp_config.to_owned(),
        "--strict-mcp-config".into(),
        // Ignore settings files inside the working directory: the agent can write there
        // and must not be able to grant itself permissions.
        "--setting-sources".into(),
        "user".into(),
        "--max-turns".into(),
        cfg.max_turns.to_string(),
    ]);
    if let Some(settings) = &cfg.settings {
        args.extend(["--settings".into(), settings.display().to_string()]);
    }
    if let Some(prompt) = &cfg.system_prompt {
        args.extend([
            "--append-system-prompt-file".into(),
            prompt.display().to_string(),
        ]);
    }
    if let Some(model) = &cfg.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = effort {
        args.extend(["--effort".into(), effort.as_str().into()]);
    }
    args.extend([
        if resume { "--resume" } else { "--session-id" }.into(),
        cfg.session_id.clone(),
    ]);
    args
}

impl Claude {
    pub fn spawn(
        cfg: &Config,
        mcp_config: &str,
        resume: bool,
        effort: Option<Effort>,
        generation: u64,
        events: mpsc::UnboundedSender<ChildEvent>,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(&cfg.claude_bin);
        command
            .args(args(cfg, mcp_config, resume, effort))
            .current_dir(&cfg.work_dir)
            .env("DISABLE_AUTOUPDATER", "1")
            .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
            // A turn is "one user message in, one result out". Background commands and
            // scheduled tasks would have Claude start turns on its own, which the
            // session's bookkeeping, and a chat front end, have no place for.
            .env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1")
            .env("CLAUDE_CODE_DISABLE_CRON", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Naming the default directory explicitly is not the same as leaving the variable
        // unset: Claude then keeps its global config file inside it instead of in `$HOME`.
        // On a developer's machine that would leave a second, stray config behind.
        let default_dir = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"));
        if default_dir.as_ref() != Some(&cfg.claude_config_dir) {
            command.env("CLAUDE_CONFIG_DIR", &cfg.claude_config_dir);
        }
        if let Some(upstream) = &cfg.upstream {
            command
                .env("ANTHROPIC_BASE_URL", format!("{}/llm", upstream.origin))
                .env("ANTHROPIC_AUTH_TOKEN", &upstream.token)
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        }
        if let Some(model) = &cfg.model {
            command.env("ANTHROPIC_MODEL", model);
        }
        if let Some(model) = &cfg.small_model {
            command.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", model);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start {}", cfg.claude_bin.display()))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let pid = child.id();
        tokio::spawn(read_output(child, generation, events));
        Ok(Self { stdin, pid, effort })
    }

    async fn write_line(&mut self, line: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(line)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await
    }

    /// Has the messages sent after this answered at `effort`, where `None` is the CLI's
    /// default for the model. Nothing is written to a process that is at that level
    /// already.
    ///
    /// Claude reads its input in order, so the change is in place before the next message
    /// without waiting for the `control_response`; see [`refused_control_request`].
    pub async fn set_effort(&mut self, effort: Option<Effort>) -> std::io::Result<()> {
        if effort == self.effort {
            return Ok(());
        }
        self.write_line(&json!({
            "type": "control_request",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "request": {
                "subtype": "apply_flag_settings",
                // `null` takes the setting away again, `--effort` included.
                "settings": { "effortLevel": effort },
            },
        }))
        .await?;
        self.effort = effort;
        Ok(())
    }

    /// Queues a user message. Claude echoes it back under `uuid` when it starts on it and
    /// ends its answer with a `result` message.
    pub async fn send_user_message(&mut self, uuid: &str, text: &str) -> std::io::Result<()> {
        self.write_line(&json!({
            "type": "user",
            "uuid": uuid,
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        }))
        .await
    }

    /// Ends the running turn the way Ctrl-C would. Claude still records a result for it.
    pub fn interrupt(&self) {
        self.signal(Signal::SIGINT);
    }

    /// Forces the process to exit. The exit is reported like any other.
    pub fn kill(&self) {
        self.signal(Signal::SIGKILL);
    }

    fn signal(&self, signal: Signal) {
        let Some(pid) = self.pid else { return };
        if let Err(error) = kill(Pid::from_raw(pid as i32), signal) {
            tracing::warn!(%error, "failed to send {signal} to claude");
        }
    }
}

/// Whether `line` is Claude's acknowledgment of the user message sent under `uuid`.
///
/// Claude can produce `result` messages nobody asked for, for instance when a resumed
/// session still had a notification queued. Only a `result` that follows this
/// acknowledgment belongs to the message.
pub fn is_replay_of(line: &Value, uuid: &str) -> bool {
    line["type"] == "user" && line["isReplay"] == true && line["uuid"] == uuid
}

/// What a piece of the model's response means for the text Claude is writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// A new text begins, so whatever was being written before is finished.
    Begin,
    /// More of the text.
    More(&'a str),
}

/// Whether `line` is a piece of a response on its way in, which Claude prints for every
/// event of the model's stream, and not a whole message. A long answer comes in thousands
/// of them, and a file being written comes twice: in pieces and then as the tool call.
pub fn is_stream_event(line: &Value) -> bool {
    line["type"] == "stream_event"
}

/// Reads such a line, which wraps the model's event in `event`. `None` for all that is
/// not text of the agent's own: thinking, the input of a tool call, and anything a
/// sub-agent writes, which is never the answer.
pub fn piece(line: &Value) -> Option<Piece<'_>> {
    if !line["parent_tool_use_id"].is_null() {
        return None;
    }
    let event = &line["event"];
    match event["type"].as_str()? {
        "message_start" => Some(Piece::Begin),
        "content_block_start" if event["content_block"]["type"] == "text" => Some(Piece::Begin),
        "content_block_delta" if event["delta"]["type"] == "text_delta" => {
            event["delta"]["text"].as_str().map(Piece::More)
        }
        _ => None,
    }
}

/// Why Claude did not do what a control request asked, if `line` is its answer to one and
/// says so. The only such request is the one in [`Claude::set_effort`], and a CLI that
/// stopped taking it would otherwise go on at the old level without a word.
pub fn refused_control_request(line: &Value) -> Option<&str> {
    let response = &line["response"];
    (line["type"] == "control_response" && response["subtype"] == "error")
        .then(|| response["error"].as_str().unwrap_or("no reason given"))
}

/// Forwards stdout line by line, then reports how the process ended. Sending the exit
/// from the same task guarantees it arrives after the last line.
async fn read_output(mut child: Child, generation: u64, events: mpsc::UnboundedSender<ChildEvent>) {
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Read beside stdout, so that the process is never held up by a pipe nobody empties.
    // The task ends with the last lines, which say why a process that died did.
    let stderr_task = tokio::spawn(async move {
        let mut tail = VecDeque::with_capacity(STDERR_TAIL_LINES);
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "claude::stderr", "{line}");
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail
    });

    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Ok(line) => {
                    if events.send(ChildEvent::Line { generation, line }).is_err() {
                        return;
                    }
                }
                Err(_) => tracing::warn!("claude wrote a non-JSON line: {line}"),
            },
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "reading claude stdout failed");
                break;
            }
        }
    }

    let status = match child.wait().await {
        Ok(status) => status.to_string(),
        Err(error) => format!("unknown ({error})"),
    };
    let stderr = Vec::from(stderr_task.await.unwrap_or_default()).join("\n");
    let detail = if stderr.is_empty() {
        format!("claude exited: {status}")
    } else {
        format!("claude exited: {status}\n{stderr}")
    };
    let _ = events.send(ChildEvent::Exited { generation, detail });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_event(event: Value, parent: Value) -> Value {
        json!({ "type": "stream_event", "event": event, "parent_tool_use_id": parent })
    }

    fn delta(delta: Value) -> Value {
        json!({ "type": "content_block_delta", "index": 0, "delta": delta })
    }

    #[test]
    fn only_the_agents_own_text_counts_as_what_it_is_writing() {
        let text = delta(json!({ "type": "text_delta", "text": "Here " }));
        let own = stream_event(text.clone(), Value::Null);
        assert!(is_stream_event(&own));
        assert_eq!(piece(&own), Some(Piece::More("Here ")));
        // The same from a sub-agent, whose text ends up in a tool result.
        assert_eq!(piece(&stream_event(text, json!("toolu_1"))), None);

        let thinking = delta(json!({ "type": "thinking_delta", "thinking": "hm" }));
        assert_eq!(piece(&stream_event(thinking, Value::Null)), None);
        let input = delta(json!({ "type": "input_json_delta", "partial_json": "{" }));
        assert_eq!(piece(&stream_event(input, Value::Null)), None);

        let start = |block: &str| {
            let start =
                json!({ "type": "content_block_start", "content_block": { "type": block } });
            stream_event(start, Value::Null)
        };
        assert_eq!(piece(&start("text")), Some(Piece::Begin));
        assert_eq!(piece(&start("tool_use")), None);
        let message = stream_event(json!({ "type": "message_start" }), Value::Null);
        assert_eq!(piece(&message), Some(Piece::Begin));

        let whole = json!({ "type": "assistant", "message": { "content": [] } });
        assert!(!is_stream_event(&whole));
    }
}
