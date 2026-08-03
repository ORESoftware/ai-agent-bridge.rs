use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, OpenOptions},
    io::Write,
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use reqwest::{redirect::Policy, Client, Response as HttpResponse, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::Semaphore};
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing::{info, warn};

use crate::slack_project_bindings::{
    AgentMode, ChannelProjectBinding, RequestedCapability, ResolveRequest, SlackProjectRegistry,
    SlackProjectRegistryDocument, WritePolicy,
};

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8151;
const DEFAULT_CONTEXT_MESSAGES: usize = 5;
const DEFAULT_BRIDGE_URL: &str = "http://127.0.0.1:8142/";
const DEFAULT_COORDINATOR_URL: &str = "http://127.0.0.1:8160/";
const DEFAULT_SLACK_API_BASE_URL: &str = "https://slack.com/api/";
const DEFAULT_CLAUDE_AGENT: &str = "claude-fable-5";
const DEFAULT_CHATGPT_AGENT: &str = "gpt-5.6-sol";
const DEFAULT_LINEAR_RUN_PROJECT: &str = "72e891e2-603d-4903-8d08-bd06d204520f";
const CALLBACK_ID: &str = "ores-agent-run-v1";
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_PROMPT_BYTES: usize = 100_000;
const MAX_CONTEXT_MESSAGES: usize = 20;
const MAX_CONTEXT_MESSAGE_BYTES: usize = 4_000;
const MAX_CONTEXT_TOTAL_BYTES: usize = 32_000;
const MAX_REMOTE_RESPONSE_BYTES: usize = 1_048_576;
const MAX_SLACK_RESPONSE_BYTES: usize = 65_536;
const MAX_IDENTIFIER_BYTES: usize = 255;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Config(String),
    #[error("invalid Slack request")]
    Request,
    #[error("request denied by channel policy")]
    Policy,
    #[error("Slack API failed")]
    Slack,
    #[error("coordinator API failed")]
    Coordinator,
    #[error("bridge API failed")]
    Bridge,
    #[error("run journal failed")]
    Journal,
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Provider {
    Claude,
    Chatgpt,
}

impl Provider {
    fn from_command(command: &str) -> Option<Self> {
        match command.trim() {
            "/ores-claude" | "/x-claude" | "/my-claude" => Some(Self::Claude),
            "/ores-chatgpt" | "/x-chatgpt" | "/my-chatgpt" => Some(Self::Chatgpt),
            _ => None,
        }
    }

    fn mode(self) -> AgentMode {
        match self {
            Self::Claude => AgentMode::Claude,
            Self::Chatgpt => AgentMode::Chatgpt,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Chatgpt => "ChatGPT",
        }
    }
}

#[derive(Clone)]
struct Config {
    host: IpAddr,
    port: u16,
    signing_secret: String,
    bot_token: String,
    registry_path: PathBuf,
    state_dir: PathBuf,
    bridge_url: String,
    bridge_bearer: Option<String>,
    coordinator_url: String,
    coordinator_bearer: Option<String>,
    slack_api_base_url: String,
    claude_agent: String,
    chatgpt_agent: String,
    linear_run_project_id: String,
    context_messages: usize,
    dry_run: bool,
    max_concurrent_runs: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        let host = env_or("SLACK_COMMAND_HOST", DEFAULT_HOST)
            .parse::<IpAddr>()
            .map_err(|_| Error::Config("SLACK_COMMAND_HOST must be an IP address".into()))?;
        let port = env_u64(
            "SLACK_COMMAND_PORT",
            DEFAULT_PORT as u64,
            1,
            u16::MAX as u64,
        )? as u16;
        let signing_secret = required("SLACK_SIGNING_SECRET")?;
        let bot_token = required("SLACK_BOT_TOKEN")?;
        let registry_path = absolute_path("SLACK_PROJECT_REGISTRY_PATH")?;
        let state_dir = env_opt("SLACK_COMMAND_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/slack-command/runs"));
        if !state_dir.is_absolute() {
            return Err(Error::Config(
                "SLACK_COMMAND_STATE_DIR must be absolute".into(),
            ));
        }
        let bridge_url = service_url(&env_or("SLACK_BRIDGE_URL", DEFAULT_BRIDGE_URL))?;
        let bridge_bearer = env_opt("SLACK_BRIDGE_BEARER");
        if !loopback_url(&bridge_url)? && bridge_bearer.is_none() {
            return Err(Error::Config(
                "SLACK_BRIDGE_BEARER is required for remote bridge URLs".into(),
            ));
        }
        let coordinator_url =
            service_url(&env_or("SLACK_COORDINATOR_URL", DEFAULT_COORDINATOR_URL))?;
        let coordinator_bearer = env_opt("SLACK_COORDINATOR_BEARER");
        if !loopback_url(&coordinator_url)? && coordinator_bearer.is_none() {
            return Err(Error::Config(
                "SLACK_COORDINATOR_BEARER is required for remote coordinator URLs".into(),
            ));
        }
        let slack_api_base_url = slack_api_base_url(&env_or(
            "SLACK_API_BASE_URL",
            DEFAULT_SLACK_API_BASE_URL,
        ))?;
        let context_messages = env_usize(
            "SLACK_CONTEXT_MESSAGE_COUNT",
            DEFAULT_CONTEXT_MESSAGES,
            0,
            MAX_CONTEXT_MESSAGES,
        )?;
        if ![0, 5, 10, 20].contains(&context_messages) {
            return Err(Error::Config(
                "SLACK_CONTEXT_MESSAGE_COUNT must be 0, 5, 10, or 20".into(),
            ));
        }
        Ok(Self {
            host,
            port,
            signing_secret,
            bot_token,
            registry_path,
            state_dir,
            bridge_url,
            bridge_bearer,
            coordinator_url,
            coordinator_bearer,
            slack_api_base_url,
            claude_agent: identifier(
                "SLACK_CLAUDE_AGENT_KEY",
                &env_or("SLACK_CLAUDE_AGENT_KEY", DEFAULT_CLAUDE_AGENT),
            )?,
            chatgpt_agent: identifier(
                "SLACK_CHATGPT_AGENT_KEY",
                &env_or("SLACK_CHATGPT_AGENT_KEY", DEFAULT_CHATGPT_AGENT),
            )?,
            linear_run_project_id: identifier(
                "SLACK_LINEAR_RUN_PROJECT_ID",
                &env_or("SLACK_LINEAR_RUN_PROJECT_ID", DEFAULT_LINEAR_RUN_PROJECT),
            )?,
            context_messages,
            dry_run: env_bool("SLACK_COMMAND_DRY_RUN", true)?,
            max_concurrent_runs: env_usize("SLACK_COMMAND_MAX_CONCURRENT_RUNS", 8, 1, 128)?,
        })
    }

    fn agent_key(&self, provider: Provider) -> &str {
        match provider {
            Provider::Claude => &self.claude_agent,
            Provider::Chatgpt => &self.chatgpt_agent,
        }
    }

    fn slack_url(&self, path: &str) -> Result<Url> {
        Url::parse(&self.slack_api_base_url)
            .and_then(|base| base.join(path))
            .map_err(|_| Error::Config("invalid Slack API URL".into()))
    }
}

fn env_opt(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

fn required(key: &str) -> Result<String> {
    env_opt(key).ok_or_else(|| Error::Config(format!("{key} must be set")))
}

fn env_bool(key: &str, default: bool) -> Result<bool> {
    match env_opt(key).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON") => Ok(true),
        Some("0" | "false" | "FALSE" | "no" | "NO" | "off" | "OFF") => Ok(false),
        Some(_) => Err(Error::Config(format!("{key} must be a boolean"))),
    }
}

fn env_u64(key: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64> {
    let value = env_opt(key)
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| Error::Config(format!("{key} must be an integer")))?
        .unwrap_or(default);
    if !(minimum..=maximum).contains(&value) {
        return Err(Error::Config(format!("{key} is outside the allowed range")));
    }
    Ok(value)
}

fn env_usize(key: &str, default: usize, minimum: usize, maximum: usize) -> Result<usize> {
    Ok(env_u64(key, default as u64, minimum as u64, maximum as u64)? as usize)
}

fn absolute_path(key: &str) -> Result<PathBuf> {
    let path = PathBuf::from(required(key)?);
    if !path.is_absolute() {
        return Err(Error::Config(format!("{key} must be absolute")));
    }
    Ok(path)
}

#[cfg(test)]
mod provider_command_alias_tests {
    use super::*;

    #[test]
    fn reviewed_aliases_map_to_the_expected_provider() {
        for command in ["/ores-claude", "/x-claude", "/my-claude"] {
            assert_eq!(Provider::from_command(command), Some(Provider::Claude));
        }
        for command in ["/ores-chatgpt", "/x-chatgpt", "/my-chatgpt"] {
            assert_eq!(Provider::from_command(command), Some(Provider::Chatgpt));
        }
    }

    #[test]
    fn unknown_or_lookalike_commands_are_rejected() {
        for command in [
            "/claude",
            "/chatgpt",
            "/ores-claude-extra",
            "/ores-chatgpt-extra",
            "/x_claude",
            "/my_chatgpt",
        ] {
            assert_eq!(Provider::from_command(command), None);
        }
    }
}
