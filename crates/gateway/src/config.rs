//! Deployment settings, read from the Worker's variables and secrets.

use worker::{Env, Result};

use crate::platform::error;

/// How the LLM gateway wants its credential presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmAuth {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedUsers {
    Anyone,
    Only(Vec<i64>),
}

impl AllowedUsers {
    /// `*` admits everyone; otherwise a comma separated list of Telegram user ids.
    /// Unset admits nobody, since every message costs the operator tokens.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("*") => Self::Anyone,
            Some(list) => Self::Only(
                list.split(',')
                    .filter_map(|id| id.trim().parse().ok())
                    .collect(),
            ),
            None => Self::Only(Vec::new()),
        }
    }

    pub fn admits(&self, user_id: i64) -> bool {
        match self {
            Self::Anyone => true,
            Self::Only(ids) => ids.contains(&user_id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Origin agent containers reach this Worker at, without a trailing slash.
    pub public_origin: String,
    /// Bot API server, normally Telegram's own.
    pub telegram_api: String,
    pub bot_token: String,
    pub webhook_secret: String,
    /// Key for signing session tokens.
    pub token_secret: String,
    /// Base URL of an Anthropic Messages compatible gateway, without `/v1`.
    pub llm_base_url: String,
    pub llm_api_key: String,
    pub llm_auth: LlmAuth,
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub allowed_users: AllowedUsers,
    /// How long an agent container stays warm after a turn.
    pub idle_ttl_ms: u64,
    pub approval_timeout_ms: u64,
    pub turn_timeout_ms: u64,
    /// Fresh (non cache-read) tokens a chat may use per UTC day. Zero means unlimited.
    pub daily_token_budget: u64,
    pub mcp_idle_ttl_ms: u64,
    /// Development only: a runner started by hand that every session uses in place of
    /// its container, e.g. `http://127.0.0.1:8080`. See the README.
    pub dev_runner_url: Option<String>,
    /// Development only: an MCP container's port served by processes started by hand,
    /// e.g. `http://127.0.0.1:8090`, used in place of the shared MCP container.
    pub dev_mcp_url: Option<String>,
}

fn var(env: &Env, name: &str) -> Option<String> {
    let value = env.var(name).ok()?.to_string();
    (!value.trim().is_empty()).then_some(value)
}

fn required(env: &Env, name: &str) -> Result<String> {
    var(env, name).ok_or_else(|| error(format!("variable {name} is not set")))
}

fn secret(env: &Env, name: &str) -> Result<String> {
    env.secret(name)
        .map(|secret| secret.to_string())
        .map_err(|_| error(format!("secret {name} is not set")))
}

fn seconds(env: &Env, name: &str, default: u64) -> Result<u64> {
    match var(env, name) {
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map(|seconds| seconds * 1000)
            .map_err(|_| error(format!("variable {name} is not a number of seconds"))),
        None => Ok(default * 1000),
    }
}

impl Config {
    pub fn load(env: &Env) -> Result<Self> {
        Ok(Self {
            public_origin: required(env, "PUBLIC_ORIGIN")?
                .trim_end_matches('/')
                .to_owned(),
            telegram_api: var(env, "TELEGRAM_API_BASE")
                .unwrap_or_else(|| "https://api.telegram.org".into())
                .trim_end_matches('/')
                .to_owned(),
            bot_token: secret(env, "TELEGRAM_BOT_TOKEN")?,
            webhook_secret: secret(env, "TELEGRAM_WEBHOOK_SECRET")?,
            token_secret: secret(env, "SESSION_TOKEN_SECRET")?,
            llm_base_url: required(env, "LLM_BASE_URL")?
                .trim_end_matches('/')
                .to_owned(),
            llm_api_key: secret(env, "LLM_API_KEY")?,
            llm_auth: match var(env, "LLM_AUTH_STYLE").as_deref() {
                None | Some("bearer") => LlmAuth::Bearer,
                Some("x-api-key") => LlmAuth::ApiKey,
                Some(other) => {
                    return Err(error(format!(
                        "LLM_AUTH_STYLE must be bearer or x-api-key, not {other}"
                    )));
                }
            },
            model: var(env, "MODEL"),
            small_model: var(env, "SMALL_MODEL"),
            allowed_users: AllowedUsers::parse(var(env, "ALLOWED_TG_USERS").as_deref()),
            idle_ttl_ms: seconds(env, "IDLE_TTL_SECS", 120)?,
            approval_timeout_ms: seconds(env, "APPROVAL_TIMEOUT_SECS", 600)?,
            turn_timeout_ms: seconds(env, "TURN_TIMEOUT_SECS", 1800)?,
            daily_token_budget: match var(env, "DAILY_TOKEN_BUDGET") {
                Some(value) => value
                    .trim()
                    .parse()
                    .map_err(|_| error("DAILY_TOKEN_BUDGET is not a number"))?,
                None => 0,
            },
            mcp_idle_ttl_ms: seconds(env, "MCP_IDLE_TTL_SECS", 900)?,
            dev_runner_url: var(env, "DEV_RUNNER_URL")
                .map(|url| url.trim_end_matches('/').to_owned()),
            dev_mcp_url: var(env, "DEV_MCP_URL").map(|url| url.trim_end_matches('/').to_owned()),
        })
    }
}
