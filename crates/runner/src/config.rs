use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use agent_proto::{RUNNER_PORT, env as proto_env};
use anyhow::Context;

/// Where the runner finds things. Everything has a default that matches the agent image;
/// the `RUNNER_*` overrides exist for local runs and tests.
#[derive(Debug, Clone)]
pub struct Config {
    /// Control surface for the Durable Object.
    pub listen: SocketAddr,
    /// Loopback-only MCP endpoint Claude calls for permission prompts.
    pub approver_listen: SocketAddr,
    pub claude_bin: PathBuf,
    /// Claude's working directory. Persisted between container starts.
    pub work_dir: PathBuf,
    /// Deliverables written here are reported as artifacts.
    pub out_dir: PathBuf,
    /// Claude's config directory; transcripts live in `projects/` beneath it.
    pub claude_config_dir: PathBuf,
    /// Scratch space for state archives on their way in and out.
    pub runtime_dir: PathBuf,
    /// `--settings` file holding the permission allow-list, if present.
    pub settings: Option<PathBuf>,
    /// `--append-system-prompt-file`, if present.
    pub system_prompt: Option<PathBuf>,
    pub max_turns: u32,
    /// Built-in tools Claude may use, as a `--tools` list. MCP tools are not affected.
    pub tools: String,
    pub session_id: String,
    /// Absent in local runs, where Claude uses whatever credentials it already has.
    pub upstream: Option<Upstream>,
    pub model: Option<String>,
    pub small_model: Option<String>,
    /// Largest state archive `/state` will hand out.
    pub state_max_bytes: u64,
}

/// What a hosted chat agent needs: a shell, files, skills and clarifying questions.
/// Left out on purpose are web access, subagents and anything that schedules work.
pub const DEFAULT_TOOLS: &str = "Bash,Read,Edit,Write,Skill,AskUserQuestion";

/// The Worker, which fronts both the LLM gateway and the shared MCP server.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub origin: String,
    pub token: String,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn path_var(name: &str, default: impl Into<PathBuf>) -> PathBuf {
    var(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into())
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.exists().then_some(path)
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let home = path_var("HOME", "/home/agent");
        let work_dir = path_var("RUNNER_WORK_DIR", "/work");
        let upstream = match (var(proto_env::WORKER_ORIGIN), var(proto_env::SESSION_TOKEN)) {
            (Some(origin), Some(token)) => Some(Upstream {
                origin: origin.trim_end_matches('/').to_owned(),
                token,
            }),
            (None, None) => None,
            _ => anyhow::bail!(
                "{} and {} must be set together",
                proto_env::WORKER_ORIGIN,
                proto_env::SESSION_TOKEN
            ),
        };
        Ok(Self {
            listen: var("RUNNER_LISTEN")
                .map(|addr| addr.parse().context("RUNNER_LISTEN"))
                .transpose()?
                .unwrap_or_else(|| ([0, 0, 0, 0], RUNNER_PORT).into()),
            approver_listen: var("RUNNER_APPROVER_LISTEN")
                .map(|addr| addr.parse().context("RUNNER_APPROVER_LISTEN"))
                .transpose()?
                .unwrap_or_else(|| ([127, 0, 0, 1], 8081).into()),
            claude_bin: path_var("RUNNER_CLAUDE_BIN", "claude"),
            out_dir: path_var("RUNNER_OUT_DIR", work_dir.join("out")),
            work_dir,
            claude_config_dir: path_var("CLAUDE_CONFIG_DIR", home.join(".claude")),
            // Under the home directory rather than /run or /tmp: those are often memory
            // backed or remounted empty when a container starts, and archives are large.
            runtime_dir: path_var("RUNNER_RUNTIME_DIR", home.join(".cache/agent-runner")),
            settings: existing(path_var("RUNNER_SETTINGS", "/etc/agent/settings.json")),
            system_prompt: existing(path_var("RUNNER_SYSTEM_PROMPT", "/etc/agent/system.md")),
            max_turns: var("RUNNER_MAX_TURNS")
                .map(|turns| turns.parse().context("RUNNER_MAX_TURNS"))
                .transpose()?
                .unwrap_or(60),
            tools: var("RUNNER_TOOLS").unwrap_or_else(|| DEFAULT_TOOLS.to_owned()),
            session_id: var(proto_env::CLAUDE_SESSION_ID)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            upstream,
            model: var(proto_env::MODEL),
            small_model: var(proto_env::SMALL_MODEL),
            state_max_bytes: var("RUNNER_STATE_MAX_BYTES")
                .map(|bytes| bytes.parse().context("RUNNER_STATE_MAX_BYTES"))
                .transpose()?
                .unwrap_or(512 * 1024 * 1024),
        })
    }

    /// Directory holding Claude's session transcripts.
    pub fn projects_dir(&self) -> PathBuf {
        self.claude_config_dir.join("projects")
    }

    /// Whether a transcript for this session is already on disk, i.e. whether Claude
    /// should resume rather than start fresh.
    pub fn has_transcript(&self) -> bool {
        let file = format!("{}.jsonl", self.session_id);
        std::fs::read_dir(self.projects_dir())
            .into_iter()
            .flatten()
            .flatten()
            .any(|project| project.path().join(&file).is_file())
    }

    pub(crate) fn dirs_to_create(&self) -> [&Path; 4] {
        [
            &self.work_dir,
            &self.out_dir,
            &self.runtime_dir,
            &self.claude_config_dir,
        ]
    }
}
