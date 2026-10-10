//! The `claude` child process: how it is launched and how its stream-json output is read.

use std::{collections::VecDeque, path::PathBuf, process::Stdio, sync::Arc};

use anyhow::Context;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc},
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
}

/// Contents of the `--mcp-config` file.
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

/// Arguments for a long-lived, stream-json driven `claude -p`.
pub fn args(cfg: &Config, resume: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        // Whole messages only. Token-by-token deltas (`--include-partial-messages`) would
        // multiply the frames sent to the Durable Object, and nothing shows them yet.
        "--verbose",
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
        cfg.mcp_config_path().display().to_string(),
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
    args.extend([
        if resume { "--resume" } else { "--session-id" }.into(),
        cfg.session_id.clone(),
    ]);
    args
}

impl Claude {
    pub fn spawn(
        cfg: &Config,
        resume: bool,
        generation: u64,
        events: mpsc::UnboundedSender<ChildEvent>,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(&cfg.claude_bin);
        command
            .args(args(cfg, resume))
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
        Ok(Self { stdin, pid })
    }

    /// Queues a user message. Claude echoes it back under `uuid` when it starts on it and
    /// ends its answer with a `result` message.
    pub async fn send_user_message(&mut self, uuid: &str, text: &str) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(&json!({
            "type": "user",
            "uuid": uuid,
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        }))?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await
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

/// Forwards stdout line by line, then reports how the process ended. Sending the exit
/// from the same task guarantees it arrives after the last line.
async fn read_output(mut child: Child, generation: u64, events: mpsc::UnboundedSender<ChildEvent>) {
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
    let stderr_task = tokio::spawn({
        let tail = tail.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "claude::stderr", "{line}");
                let mut tail = tail.lock().await;
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        }
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
    let _ = stderr_task.await;
    let stderr = Vec::from(std::mem::take(&mut *tail.lock().await)).join("\n");
    let detail = if stderr.is_empty() {
        format!("claude exited: {status}")
    } else {
        format!("claude exited: {status}\n{stderr}")
    };
    let _ = events.send(ChildEvent::Exited { generation, detail });
}
