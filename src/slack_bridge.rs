//! Authenticated Slack ingress/egress for the existing bridge workflow API.
//!
//! This module intentionally contains no model-provider SDKs. A valid Slack
//! command creates one competitive workflow for exactly two configured bridge
//! agent identities, then posts each resulting submission back to the originating
//! Slack thread. Request authentication, allowlists, replay protection, bounded
//! concurrency, and a durable event journal stay at this adapter boundary.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    net::IpAddr,
    path::{Path, PathBuf},
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
use parking_lot::Mutex;
use reqwest::{redirect::Policy, Client, Response as HttpResponse, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{sleep, Instant},
};
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing::{info, info_span, warn, Instrument};

mod commands;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8150;
const DEFAULT_COMMAND_PREFIX: &str = "!ask-both";
const DEFAULT_CLAUDE_AGENT_KEY: &str = "claude-fable-5";
const DEFAULT_OPENAI_AGENT_KEY: &str = "gpt-5.6-sol";
const DEFAULT_BRIDGE_URL: &str = "http://127.0.0.1:8142/";
const DEFAULT_MAX_REQUEST_AGE_SECS: u64 = 300;
const DEFAULT_WORKFLOW_TIMEOUT_SECS: u64 = 120;
const DEFAULT_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_MAX_BODY_BYTES: usize = 262_144;
const DEFAULT_MAX_CONCURRENT_WORKFLOWS: usize = 8;
const MAX_REQUEST_AGE_SECS: u64 = 300;
const MAX_WORKFLOW_TIMEOUT_SECS: u64 = 900;
const MAX_BODY_BYTES: usize = 1_048_576;
const MAX_PROMPT_BYTES: usize = 100_000;
const MAX_SLACK_MESSAGE_BYTES: usize = 35_000;
const MAX_REMOTE_RESPONSE_BYTES: usize = 1_048_576;
const MAX_SLACK_RESPONSE_BYTES: usize = 65_536;
const MAX_EVENT_ID_BYTES: usize = 255;
const MAX_IDENTIFIER_BYTES: usize = 255;
const MAX_PREFIX_BYTES: usize = 128;
const SLACK_POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";
const SLACK_VIEWS_OPEN_URL: &str = "https://slack.com/api/views.open";
const SLACK_CONVERSATIONS_HISTORY_URL: &str = "https://slack.com/api/conversations.history";

#[derive(Debug, thiserror::Error)]
enum AdapterError {
    #[error("{0}")]
    Configuration(String),
    #[error("persistent event journal is unavailable")]
    Journal,
    #[error("bridge request failed")]
    Bridge,
    #[error("Slack reply failed")]
    Slack,
}

type AdapterResult<T> = Result<T, AdapterError>;

#[derive(Clone)]
struct SlackConfig {
    host: IpAddr,
    port: u16,
    signing_secret: String,
    bot_token: Option<String>,
    bot_user_id: Option<String>,
    allowed_team_ids: BTreeSet<String>,
    allowed_channel_ids: BTreeSet<String>,
    allowed_thread_ts: BTreeSet<String>,
    command_prefix: String,
    bridge_url: String,
    bridge_bearer: Option<String>,
    slack_post_message_url: String,
    claude_agent_key: String,
    openai_agent_key: String,
    dry_run: bool,
    idempotency_path: PathBuf,
    max_request_age_secs: u64,
    workflow_timeout: Duration,
    poll_interval: Duration,
    max_body_bytes: usize,
    max_concurrent_workflows: usize,
    // Slash-command surface (`/my-claude`, `/my-chatgpt`). See `commands`.
    claude_command: String,
    openai_command: String,
    claude_model_choices: Vec<String>,
    openai_model_choices: Vec<String>,
    target_choices: Vec<String>,
    context_message_default: usize,
    context_message_max: usize,
    slack_views_open_url: String,
    slack_conversations_history_url: String,
    broadcast_channel_id: Option<String>,
    linear_api_key: Option<String>,
    linear_team_id: Option<String>,
    linear_project_id: Option<String>,
    linear_state_todo: Option<String>,
    linear_state_started: Option<String>,
    linear_state_done: Option<String>,
}

impl SlackConfig {
    fn from_env() -> AdapterResult<Self> {
        let host = env_or("SLACK_BRIDGE_HOST", DEFAULT_HOST)
            .parse::<IpAddr>()
            .map_err(|_| {
                AdapterError::Configuration(
                    "SLACK_BRIDGE_HOST must be a valid IP address".to_string(),
                )
            })?;
        let port = env_u64("SLACK_BRIDGE_PORT", DEFAULT_PORT as u64, 1, u16::MAX as u64)? as u16;
        let signing_secret = required_secret("SLACK_SIGNING_SECRET")?;
        if signing_secret.len() < 16 {
            return Err(AdapterError::Configuration(
                "SLACK_SIGNING_SECRET is unexpectedly short".to_string(),
            ));
        }

        let dry_run = env_bool("SLACK_BRIDGE_DRY_RUN", true)?;
        let bot_token = env_opt("SLACK_BOT_TOKEN");
        if !dry_run && bot_token.is_none() {
            return Err(AdapterError::Configuration(
                "SLACK_BOT_TOKEN is required when SLACK_BRIDGE_DRY_RUN=false".to_string(),
            ));
        }

        let allowed_team_ids = required_csv_set("SLACK_ALLOWED_TEAM_IDS")?;
        let allowed_channel_ids = required_csv_set("SLACK_ALLOWED_CHANNEL_IDS")?;
        let allowed_thread_ts = optional_csv_set("SLACK_ALLOWED_THREAD_TS")?;
        let bot_user_id = env_opt("SLACK_BOT_USER_ID")
            .map(|value| normalize_identifier("SLACK_BOT_USER_ID", &value))
            .transpose()?;

        let command_prefix = env_or("SLACK_COMMAND_PREFIX", DEFAULT_COMMAND_PREFIX);
        validate_command_prefix(&command_prefix)?;

        let bridge_url = normalize_bridge_url(&env_or("SLACK_BRIDGE_URL", DEFAULT_BRIDGE_URL))?;
        let bridge_bearer = env_opt("SLACK_BRIDGE_BEARER");
        if !url_is_loopback(&bridge_url)? && bridge_bearer.is_none() {
            return Err(AdapterError::Configuration(
                "SLACK_BRIDGE_BEARER is required for a non-loopback bridge URL".to_string(),
            ));
        }

        let claude_agent_key = normalize_identifier(
            "SLACK_CLAUDE_AGENT_KEY",
            &env_or("SLACK_CLAUDE_AGENT_KEY", DEFAULT_CLAUDE_AGENT_KEY),
        )?;
        let openai_agent_key = normalize_identifier(
            "SLACK_OPENAI_AGENT_KEY",
            &env_or("SLACK_OPENAI_AGENT_KEY", DEFAULT_OPENAI_AGENT_KEY),
        )?;
        if claude_agent_key == openai_agent_key {
            return Err(AdapterError::Configuration(
                "Slack model agent keys must be distinct".to_string(),
            ));
        }

        let idempotency_path = env_opt("SLACK_IDEMPOTENCY_PATH")
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(default_idempotency_path)?;
        if !idempotency_path.is_absolute() {
            return Err(AdapterError::Configuration(
                "SLACK_IDEMPOTENCY_PATH must be absolute".to_string(),
            ));
        }

        let max_request_age_secs = env_u64(
            "SLACK_MAX_REQUEST_AGE_SECS",
            DEFAULT_MAX_REQUEST_AGE_SECS,
            1,
            MAX_REQUEST_AGE_SECS,
        )?;
        let workflow_timeout_secs = env_u64(
            "SLACK_WORKFLOW_TIMEOUT_SECS",
            DEFAULT_WORKFLOW_TIMEOUT_SECS,
            5,
            MAX_WORKFLOW_TIMEOUT_SECS,
        )?;
        let poll_interval_ms = env_u64(
            "SLACK_POLL_INTERVAL_MS",
            DEFAULT_POLL_INTERVAL_MS,
            100,
            10_000,
        )?;
        let max_body_bytes = env_usize(
            "SLACK_MAX_BODY_BYTES",
            DEFAULT_MAX_BODY_BYTES,
            1_024,
            MAX_BODY_BYTES,
        )?;
        let max_concurrent_workflows = env_usize(
            "SLACK_MAX_CONCURRENT_WORKFLOWS",
            DEFAULT_MAX_CONCURRENT_WORKFLOWS,
            1,
            128,
        )?;

        let claude_command = validate_slash_command(
            "SLACK_CLAUDE_COMMAND",
            &env_or("SLACK_CLAUDE_COMMAND", commands::DEFAULT_CLAUDE_COMMAND),
        )?;
        let openai_command = validate_slash_command(
            "SLACK_OPENAI_COMMAND",
            &env_or("SLACK_OPENAI_COMMAND", commands::DEFAULT_OPENAI_COMMAND),
        )?;
        if claude_command == openai_command {
            return Err(AdapterError::Configuration(
                "Slack slash commands must be distinct".to_string(),
            ));
        }

        // Each command may only ever dispatch keys from its own provider list,
        // so the two lists must not overlap.
        let claude_model_choices = optional_csv_list("SLACK_CLAUDE_MODEL_CHOICES")?
            .unwrap_or_else(|| vec![claude_agent_key.clone()]);
        let openai_model_choices = optional_csv_list("SLACK_OPENAI_MODEL_CHOICES")?
            .unwrap_or_else(|| vec![openai_agent_key.clone()]);
        if claude_model_choices
            .iter()
            .any(|key| openai_model_choices.contains(key))
        {
            return Err(AdapterError::Configuration(
                "SLACK_CLAUDE_MODEL_CHOICES and SLACK_OPENAI_MODEL_CHOICES must not overlap"
                    .to_string(),
            ));
        }
        let target_choices = optional_csv_list("SLACK_TARGET_CHOICES")?.unwrap_or_default();

        let context_message_max = env_usize(
            "SLACK_CONTEXT_MESSAGE_MAX",
            commands::MAX_CONTEXT_MESSAGES,
            0,
            commands::MAX_CONTEXT_MESSAGES,
        )?;
        let context_message_default = env_usize(
            "SLACK_CONTEXT_MESSAGE_DEFAULT",
            commands::DEFAULT_CONTEXT_MESSAGES.min(context_message_max),
            0,
            context_message_max,
        )?;

        let broadcast_channel_id = env_opt("SLACK_BROADCAST_CHANNEL_ID")
            .map(|value| normalize_identifier("SLACK_BROADCAST_CHANNEL_ID", &value))
            .transpose()?;

        let linear_api_key = env_opt("SLACK_LINEAR_API_KEY");
        let linear_team_id = env_opt("SLACK_LINEAR_TEAM_ID")
            .map(|value| normalize_identifier("SLACK_LINEAR_TEAM_ID", &value))
            .transpose()?;
        if linear_team_id.is_some() && linear_api_key.is_none() {
            return Err(AdapterError::Configuration(
                "SLACK_LINEAR_API_KEY is required when SLACK_LINEAR_TEAM_ID is set".to_string(),
            ));
        }
        let linear_project_id = env_opt("SLACK_LINEAR_PROJECT_ID")
            .map(|value| normalize_identifier("SLACK_LINEAR_PROJECT_ID", &value))
            .transpose()?;
        let linear_state_todo = env_opt("SLACK_LINEAR_STATE_TODO")
            .map(|value| normalize_identifier("SLACK_LINEAR_STATE_TODO", &value))
            .transpose()?;
        let linear_state_started = env_opt("SLACK_LINEAR_STATE_STARTED")
            .map(|value| normalize_identifier("SLACK_LINEAR_STATE_STARTED", &value))
            .transpose()?;
        let linear_state_done = env_opt("SLACK_LINEAR_STATE_DONE")
            .map(|value| normalize_identifier("SLACK_LINEAR_STATE_DONE", &value))
            .transpose()?;

        Ok(Self {
            host,
            port,
            signing_secret,
            bot_token,
            bot_user_id,
            allowed_team_ids,
            allowed_channel_ids,
            allowed_thread_ts,
            command_prefix,
            bridge_url,
            bridge_bearer,
            slack_post_message_url: SLACK_POST_MESSAGE_URL.to_string(),
            claude_agent_key,
            openai_agent_key,
            dry_run,
            idempotency_path,
            max_request_age_secs,
            workflow_timeout: Duration::from_secs(workflow_timeout_secs),
            poll_interval: Duration::from_millis(poll_interval_ms),
            max_body_bytes,
            max_concurrent_workflows,
            claude_command,
            openai_command,
            claude_model_choices,
            openai_model_choices,
            target_choices,
            context_message_default,
            context_message_max,
            slack_views_open_url: SLACK_VIEWS_OPEN_URL.to_string(),
            slack_conversations_history_url: SLACK_CONVERSATIONS_HISTORY_URL.to_string(),
            broadcast_channel_id,
            linear_api_key,
            linear_team_id,
            linear_project_id,
            linear_state_todo,
            linear_state_started,
            linear_state_done,
        })
    }

    fn models(&self) -> [ModelRoute<'_>; 2] {
        [
            ModelRoute {
                agent_key: &self.claude_agent_key,
                label: "Claude Fable",
            },
            ModelRoute {
                agent_key: &self.openai_agent_key,
                label: "ChatGPT 5.6 Sol",
            },
        ]
    }
}

#[derive(Clone, Copy)]
struct ModelRoute<'a> {
    agent_key: &'a str,
    label: &'static str,
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

fn required_secret(key: &str) -> AdapterResult<String> {
    env_opt(key).ok_or_else(|| {
        AdapterError::Configuration(format!("{key} must be supplied through the environment"))
    })
}

fn env_bool(key: &str, default: bool) -> AdapterResult<bool> {
    match env_opt(key).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON") => Ok(true),
        Some("0" | "false" | "FALSE" | "no" | "NO" | "off" | "OFF") => Ok(false),
        Some(_) => Err(AdapterError::Configuration(format!(
            "{key} must be a boolean"
        ))),
    }
}

fn env_u64(key: &str, default: u64, minimum: u64, maximum: u64) -> AdapterResult<u64> {
    let value = match env_opt(key) {
        Some(value) => value.parse::<u64>().map_err(|_| {
            AdapterError::Configuration(format!("{key} must be an unsigned integer"))
        })?,
        None => default,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(AdapterError::Configuration(format!(
            "{key} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn env_usize(key: &str, default: usize, minimum: usize, maximum: usize) -> AdapterResult<usize> {
    let value = match env_opt(key) {
        Some(value) => value.parse::<usize>().map_err(|_| {
            AdapterError::Configuration(format!("{key} must be an unsigned integer"))
        })?,
        None => default,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(AdapterError::Configuration(format!(
            "{key} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn required_csv_set(key: &str) -> AdapterResult<BTreeSet<String>> {
    let values = optional_csv_set(key)?;
    if values.is_empty() {
        return Err(AdapterError::Configuration(format!(
            "{key} must contain at least one identifier"
        )));
    }
    Ok(values)
}

fn optional_csv_set(key: &str) -> AdapterResult<BTreeSet<String>> {
    let mut values = BTreeSet::new();
    if let Some(raw) = env_opt(key) {
        for item in raw.split(',') {
            let item = normalize_identifier(key, item)?;
            values.insert(item);
        }
    }
    Ok(values)
}

fn normalize_identifier(name: &str, value: &str) -> AdapterResult<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(AdapterError::Configuration(format!(
            "{name} contains an invalid identifier"
        )));
    }
    Ok(value.to_string())
}

fn validate_command_prefix(value: &str) -> AdapterResult<()> {
    if value.is_empty()
        || value.len() > MAX_PREFIX_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(AdapterError::Configuration(
            "SLACK_COMMAND_PREFIX must be one non-empty token".to_string(),
        ));
    }
    Ok(())
}

fn normalize_bridge_url(value: &str) -> AdapterResult<String> {
    let mut url = Url::parse(value).map_err(|_| {
        AdapterError::Configuration("SLACK_BRIDGE_URL must be a valid URL".to_string())
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AdapterError::Configuration(
            "SLACK_BRIDGE_URL cannot contain credentials, a query, or a fragment".to_string(),
        ));
    }
    if url.scheme() != "https" && !url_host_is_loopback(&url) {
        return Err(AdapterError::Configuration(
            "SLACK_BRIDGE_URL must use HTTPS unless it targets loopback".to_string(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url.to_string())
}

fn url_is_loopback(value: &str) -> AdapterResult<bool> {
    let url = Url::parse(value).map_err(|_| {
        AdapterError::Configuration("SLACK_BRIDGE_URL must be a valid URL".to_string())
    })?;
    Ok(url_host_is_loopback(&url))
}

fn url_host_is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn default_idempotency_path() -> AdapterResult<PathBuf> {
    let root = env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".local/state"))
        })
        .ok_or_else(|| {
            AdapterError::Configuration(
                "set SLACK_IDEMPOTENCY_PATH, XDG_STATE_HOME, or HOME".to_string(),
            )
        })?;
    if !root.is_absolute() {
        return Err(AdapterError::Configuration(
            "the Slack event state directory must be absolute".to_string(),
        ));
    }
    Ok(root
        .join("fiducia-ai-agent-bridge")
        .join("slack-events.jsonl"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EventState {
    Claimed,
    WorkflowCreated,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct EventRecord {
    event_id: String,
    state: EventState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_id: Option<String>,
    #[serde(default)]
    posted_agents: BTreeSet<String>,
    updated_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClaimOutcome {
    Claimed,
    Duplicate,
}

struct EventStore {
    path: PathBuf,
    records: Mutex<BTreeMap<String, EventRecord>>,
}

impl EventStore {
    fn open(path: PathBuf) -> AdapterResult<Self> {
        prepare_private_parent(&path)?;
        let mut records = BTreeMap::new();
        if path.exists() {
            reject_symlink_or_non_file(&path)?;
            let file = open_existing_journal(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(|_| AdapterError::Journal)?;
                if line.trim().is_empty() {
                    continue;
                }
                let record = serde_json::from_str::<EventRecord>(&line)
                    .map_err(|_| AdapterError::Journal)?;
                validate_persisted_record(&record)?;
                records.insert(record.event_id.clone(), record);
            }
        }
        Ok(Self {
            path,
            records: Mutex::new(records),
        })
    }

    fn claim(&self, event_id: &str) -> AdapterResult<ClaimOutcome> {
        if !valid_event_id(event_id) {
            return Err(AdapterError::Journal);
        }
        let mut records = self.records.lock();
        if records.contains_key(event_id) {
            return Ok(ClaimOutcome::Duplicate);
        }
        let record = EventRecord {
            event_id: event_id.to_string(),
            state: EventState::Claimed,
            workflow_id: None,
            posted_agents: BTreeSet::new(),
            updated_at: Utc::now().to_rfc3339(),
        };
        self.append(&record)?;
        records.insert(event_id.to_string(), record);
        Ok(ClaimOutcome::Claimed)
    }

    fn set_workflow(&self, event_id: &str, workflow_id: &str) -> AdapterResult<()> {
        self.update(event_id, |record| {
            record.state = EventState::WorkflowCreated;
            record.workflow_id = Some(workflow_id.to_string());
        })
    }

    fn mark_posted(&self, event_id: &str, agent_key: &str) -> AdapterResult<()> {
        self.update(event_id, |record| {
            record.posted_agents.insert(agent_key.to_string());
        })
    }

    fn complete(&self, event_id: &str) -> AdapterResult<()> {
        self.update(event_id, |record| {
            record.state = EventState::Completed;
        })
    }

    fn snapshot(&self, event_id: &str) -> Option<EventRecord> {
        self.records.lock().get(event_id).cloned()
    }

    fn update(&self, event_id: &str, mutate: impl FnOnce(&mut EventRecord)) -> AdapterResult<()> {
        let mut records = self.records.lock();
        let record = records.get_mut(event_id).ok_or(AdapterError::Journal)?;
        let previous = record.clone();
        mutate(record);
        record.updated_at = Utc::now().to_rfc3339();
        if let Err(error) = self.append(record) {
            *record = previous;
            return Err(error);
        }
        Ok(())
    }

    fn append(&self, record: &EventRecord) -> AdapterResult<()> {
        let encoded = serde_json::to_string(record).map_err(|_| AdapterError::Journal)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| AdapterError::Journal)?;
        validate_open_journal(&file)?;
        file.write_all(encoded.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_data())
            .map_err(|_| AdapterError::Journal)
    }
}

fn prepare_private_parent(path: &Path) -> AdapterResult<()> {
    let parent = path.parent().ok_or(AdapterError::Journal)?;
    fs::create_dir_all(parent).map_err(|_| AdapterError::Journal)?;
    let metadata = fs::symlink_metadata(parent).map_err(|_| AdapterError::Journal)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AdapterError::Journal);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AdapterError::Journal);
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| AdapterError::Journal)?;
    }
    Ok(())
}

fn open_existing_journal(path: &Path) -> AdapterResult<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| AdapterError::Journal)?;
    validate_open_journal(&file)?;
    Ok(file)
}

fn validate_persisted_record(record: &EventRecord) -> AdapterResult<()> {
    if !valid_event_id(&record.event_id)
        || record
            .workflow_id
            .as_deref()
            .is_some_and(|workflow_id| !valid_event_id(workflow_id))
        || record.posted_agents.iter().any(|agent_key| {
            agent_key.is_empty()
                || agent_key.len() > MAX_IDENTIFIER_BYTES
                || agent_key.chars().any(char::is_control)
        })
    {
        return Err(AdapterError::Journal);
    }
    Ok(())
}

fn reject_symlink_or_non_file(path: &Path) -> AdapterResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| AdapterError::Journal)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AdapterError::Journal);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(AdapterError::Journal);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| AdapterError::Journal)?;
    }
    Ok(())
}

fn validate_open_journal(file: &File) -> AdapterResult<()> {
    let metadata = file.metadata().map_err(|_| AdapterError::Journal)?;
    if !metadata.is_file() {
        return Err(AdapterError::Journal);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(AdapterError::Journal);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct SlackEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    challenge: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AcceptedEvent {
    event_id: String,
    team_id: String,
    channel: String,
    thread_ts: String,
    user: String,
    prompt: String,
}

#[derive(Debug, Eq, PartialEq)]
enum EventDecision {
    Challenge(String),
    Ignore,
    Accept(AcceptedEvent),
    Reject,
}

fn classify_event(config: &SlackConfig, envelope: SlackEnvelope) -> EventDecision {
    if envelope.kind == "url_verification" {
        let Some(team_id) = envelope.team_id.as_deref() else {
            return EventDecision::Reject;
        };
        if !config.allowed_team_ids.contains(team_id) {
            return EventDecision::Ignore;
        }
        return match envelope.challenge {
            Some(challenge)
                if !challenge.is_empty()
                    && challenge.len() <= 1_024
                    && !challenge.chars().any(char::is_control) =>
            {
                EventDecision::Challenge(challenge)
            }
            _ => EventDecision::Reject,
        };
    }

    if envelope.kind != "event_callback" {
        return EventDecision::Ignore;
    }

    let Some(team_id) = envelope.team_id else {
        return EventDecision::Reject;
    };
    if !config.allowed_team_ids.contains(&team_id) {
        return EventDecision::Ignore;
    }

    let Some(event_id) = envelope.event_id else {
        return EventDecision::Reject;
    };
    if !valid_event_id(&event_id) {
        return EventDecision::Reject;
    }

    let Some(event) = envelope.event else {
        return EventDecision::Reject;
    };
    if event.kind != "message" && event.kind != "app_mention" {
        return EventDecision::Ignore;
    }
    if event.bot_id.is_some() || event.subtype.is_some() {
        return EventDecision::Ignore;
    }

    let Some(user) = event.user else {
        return EventDecision::Ignore;
    };
    if config
        .bot_user_id
        .as_ref()
        .is_some_and(|bot_user_id| bot_user_id == &user)
    {
        return EventDecision::Ignore;
    }

    let Some(channel) = event.channel else {
        return EventDecision::Reject;
    };
    if !config.allowed_channel_ids.contains(&channel) {
        return EventDecision::Ignore;
    }

    let Some(ts) = event.ts else {
        return EventDecision::Reject;
    };
    if !valid_slack_timestamp(&ts) {
        return EventDecision::Reject;
    }
    let thread_ts = event.thread_ts.unwrap_or(ts);
    if !valid_slack_timestamp(&thread_ts) {
        return EventDecision::Reject;
    }
    if !config.allowed_thread_ts.is_empty() && !config.allowed_thread_ts.contains(&thread_ts) {
        return EventDecision::Ignore;
    }

    let Some(text) = event.text else {
        return EventDecision::Ignore;
    };
    match parse_command(&text, &config.command_prefix) {
        CommandParse::NotCommand => EventDecision::Ignore,
        CommandParse::Invalid => EventDecision::Reject,
        CommandParse::Prompt(prompt) => EventDecision::Accept(AcceptedEvent {
            event_id,
            team_id,
            channel,
            thread_ts,
            user,
            prompt,
        }),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CommandParse {
    NotCommand,
    Invalid,
    Prompt(String),
}

fn parse_command(text: &str, prefix: &str) -> CommandParse {
    let text = strip_leading_mention(text.trim());
    let Some(rest) = text.strip_prefix(prefix) else {
        return CommandParse::NotCommand;
    };
    if rest.is_empty() {
        return CommandParse::Invalid;
    }
    if !rest
        .chars()
        .next()
        .is_some_and(|character| character.is_whitespace())
    {
        return CommandParse::NotCommand;
    }
    let prompt = rest.trim();
    if prompt.is_empty()
        || prompt.len() > MAX_PROMPT_BYTES
        || prompt.chars().any(|character| character == '\0')
    {
        return CommandParse::Invalid;
    }
    CommandParse::Prompt(prompt.to_string())
}

fn strip_leading_mention(text: &str) -> &str {
    if !text.starts_with("<@") {
        return text;
    }
    text.find('>')
        .map(|index| text[index + 1..].trim_start())
        .unwrap_or(text)
}

fn valid_slack_timestamp(value: &str) -> bool {
    let mut parts = value.split('.');
    let seconds = parts.next().is_some_and(|part| {
        !part.is_empty()
            && part.len() <= 20
            && part.chars().all(|character| character.is_ascii_digit())
    });
    let fraction = parts.next().is_some_and(|part| {
        !part.is_empty()
            && part.len() <= 12
            && part.chars().all(|character| character.is_ascii_digit())
    });
    seconds && fraction && parts.next().is_none()
}

fn valid_event_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_EVENT_ID_BYTES
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ':' | '.')
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignatureError {
    Missing,
    Timestamp,
    Stale,
    Invalid,
}

fn verify_slack_signature(
    config: &SlackConfig,
    headers: &HeaderMap,
    body: &[u8],
    now_unix: i64,
) -> Result<(), SignatureError> {
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .and_then(|value| value.to_str().ok())
        .ok_or(SignatureError::Missing)?;
    let timestamp_value = timestamp
        .parse::<i64>()
        .map_err(|_| SignatureError::Timestamp)?;
    let age = if now_unix >= timestamp_value {
        now_unix.saturating_sub(timestamp_value) as u64
    } else {
        timestamp_value.saturating_sub(now_unix) as u64
    };
    if age > config.max_request_age_secs {
        return Err(SignatureError::Stale);
    }

    let signature = headers
        .get("x-slack-signature")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("v0="))
        .ok_or(SignatureError::Missing)?;
    let signature = decode_hex_32(signature).ok_or(SignatureError::Invalid)?;

    let mut mac = HmacSha256::new_from_slice(config.signing_secret.as_bytes())
        .map_err(|_| SignatureError::Invalid)?;
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| SignatureError::Invalid)
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let bytes = value.as_bytes();
    let mut decoded = [0u8; 32];
    for index in 0..32 {
        let high = hex_nibble(bytes[index * 2])?;
        let low = hex_nibble(bytes[index * 2 + 1])?;
        decoded[index] = (high << 4) | low;
    }
    Some(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

struct SlackApp {
    config: SlackConfig,
    client: Client,
    store: EventStore,
    workflow_limit: Arc<Semaphore>,
}

impl SlackApp {
    fn new(config: SlackConfig) -> AdapterResult<Self> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent("fiducia-slack-bridge/0.1")
            .build()
            .map_err(|_| {
                AdapterError::Configuration("failed to initialize HTTP client".to_string())
            })?;
        let store = EventStore::open(config.idempotency_path.clone())?;
        let workflow_limit = Arc::new(Semaphore::new(config.max_concurrent_workflows));
        Ok(Self {
            config,
            client,
            store,
            workflow_limit,
        })
    }

    async fn create_workflow(&self, event: &AcceptedEvent) -> AdapterResult<String> {
        let url = Url::parse(&self.config.bridge_url)
            .and_then(|base| base.join("workflows"))
            .map_err(|_| AdapterError::Bridge)?;
        let payload = workflow_create_payload(&self.config, event);
        let mut request = self.client.post(url).json(&payload);
        if let Some(token) = &self.config.bridge_bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| AdapterError::Bridge)?;
        let status = response.status();
        let body = read_bounded(response, MAX_REMOTE_RESPONSE_BYTES)
            .await
            .ok_or(AdapterError::Bridge)?;
        if !status.is_success() {
            return Err(AdapterError::Bridge);
        }
        let response = serde_json::from_slice::<WorkflowApiResponse>(&body)
            .map_err(|_| AdapterError::Bridge)?;
        validate_workflow_response(&self.config, &response.workflow)?;
        Ok(response.workflow.plan.id)
    }

    async fn get_workflow(&self, workflow_id: &str) -> AdapterResult<WorkflowViewDto> {
        if !valid_event_id(workflow_id) {
            return Err(AdapterError::Bridge);
        }
        let path = format!("workflows/{workflow_id}");
        let url = Url::parse(&self.config.bridge_url)
            .and_then(|base| base.join(&path))
            .map_err(|_| AdapterError::Bridge)?;
        let mut request = self.client.get(url);
        if let Some(token) = &self.config.bridge_bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| AdapterError::Bridge)?;
        let status = response.status();
        let body = read_bounded(response, MAX_REMOTE_RESPONSE_BYTES)
            .await
            .ok_or(AdapterError::Bridge)?;
        if !status.is_success() {
            return Err(AdapterError::Bridge);
        }
        let response = serde_json::from_slice::<WorkflowApiResponse>(&body)
            .map_err(|_| AdapterError::Bridge)?;
        validate_workflow_response(&self.config, &response.workflow)?;
        Ok(response.workflow)
    }

    async fn post_slack_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> AdapterResult<()> {
        let token = self.config.bot_token.as_ref().ok_or(AdapterError::Slack)?;
        let payload = json!({
            "channel": channel,
            "thread_ts": thread_ts,
            "text": truncate_utf8(text, MAX_SLACK_MESSAGE_BYTES),
            "unfurl_links": false,
            "unfurl_media": false
        });

        for attempt in 0..3 {
            let response = self
                .client
                .post(&self.config.slack_post_message_url)
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await;
            if let Ok(response) = response {
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|seconds| seconds.clamp(1, 10));
                let body = read_bounded(response, MAX_SLACK_RESPONSE_BYTES).await;
                if status.is_success()
                    && body
                        .and_then(|bytes| serde_json::from_slice::<SlackApiResponse>(&bytes).ok())
                        .is_some_and(|response| response.ok)
                {
                    return Ok(());
                }
                if attempt < 2 {
                    sleep(Duration::from_secs(retry_after.unwrap_or(1))).await;
                    continue;
                }
            } else if attempt < 2 {
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }
        Err(AdapterError::Slack)
    }
}

fn workflow_create_payload(config: &SlackConfig, event: &AcceptedEvent) -> Value {
    json!({
        "title": format!("Slack dual-model request {}", event.event_id),
        "prompt": event.prompt.as_str(),
        "created_by": config.claude_agent_key.as_str(),
        "mode": "competitive",
        "agent_keys": [
            config.claude_agent_key.as_str(),
            config.openai_agent_key.as_str()
        ],
        "worker_count": 2,
        "meta": {
            "source": "slack",
            "slack_event_id": event.event_id.as_str(),
            "slack_team_id": event.team_id.as_str(),
            "slack_channel_id": event.channel.as_str(),
            "slack_thread_ts": event.thread_ts.as_str(),
            "slack_user_id": event.user.as_str(),
            "requested_agent_count": 2
        }
    })
}

#[derive(Debug, Deserialize)]
struct WorkflowApiResponse {
    workflow: WorkflowViewDto,
}

#[derive(Debug, Deserialize)]
struct WorkflowViewDto {
    plan: WorkflowPlanDto,
    status: WorkflowStatusDto,
    #[serde(default)]
    submissions: Vec<WorkflowSubmissionDto>,
}

#[derive(Debug, Deserialize)]
struct WorkflowPlanDto {
    id: String,
    #[serde(default)]
    assignments: Vec<WorkflowAssignmentDto>,
}

#[derive(Debug, Deserialize)]
struct WorkflowAssignmentDto {
    agent_key: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowStatusDto {
    stage: String,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkflowSubmissionDto {
    agent_key: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct SlackApiResponse {
    ok: bool,
}

fn validate_workflow_response(
    config: &SlackConfig,
    workflow: &WorkflowViewDto,
) -> AdapterResult<()> {
    if !valid_event_id(&workflow.plan.id) {
        return Err(AdapterError::Bridge);
    }
    let assigned = workflow
        .plan
        .assignments
        .iter()
        .map(|assignment| assignment.agent_key.as_str())
        .collect::<BTreeSet<_>>();
    let expected = [
        config.claude_agent_key.as_str(),
        config.openai_agent_key.as_str(),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if assigned != expected || workflow.plan.assignments.len() != 2 {
        return Err(AdapterError::Bridge);
    }
    Ok(())
}

async fn read_bounded(response: HttpResponse, limit: usize) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > limit as u64)
    {
        return None;
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if output.len().saturating_add(chunk.len()) > limit {
            return None;
        }
        output.extend_from_slice(&chunk);
    }
    Some(output)
}

pub async fn run() -> anyhow::Result<()> {
    let _telemetry = fiducia_telemetry::init("fiducia-slack-bridge");
    let config = SlackConfig::from_env().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let address = std::net::SocketAddr::new(config.host, config.port);
    let app = Arc::new(SlackApp::new(config).map_err(|error| anyhow::anyhow!(error.to_string()))?);
    let router = router(app.clone());
    let listener = TcpListener::bind(address).await?;
    info!(
        %address,
        dry_run = app.config.dry_run,
        max_concurrent_workflows = app.config.max_concurrent_workflows,
        "starting authenticated Slack workflow adapter"
    );
    axum::serve(listener, router).await?;
    Ok(())
}

fn router(app: Arc<SlackApp>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/slack/events", post(slack_events))
        .layer(DefaultBodyLimit::max(app.config.max_body_bytes))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .with_state(app)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

async fn readyz(State(app): State<Arc<SlackApp>>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "dry_run": app.config.dry_run
    }))
}

async fn slack_events(
    State(app): State<Arc<SlackApp>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if verify_slack_signature(&app.config, &headers, &body, Utc::now().timestamp()).is_err() {
        emit_metric("rejected_signature");
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "ok": false, "error": "unauthorized" }),
        );
    }

    let envelope = match serde_json::from_slice::<SlackEnvelope>(&body) {
        Ok(envelope) => envelope,
        Err(_) => {
            emit_metric("rejected_malformed");
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "malformed_request" }),
            );
        }
    };

    match classify_event(&app.config, envelope) {
        EventDecision::Challenge(challenge) => {
            emit_metric("url_verification");
            json_response(StatusCode::OK, json!({ "challenge": challenge }))
        }
        EventDecision::Ignore => {
            emit_metric("ignored");
            json_response(StatusCode::OK, json!({ "ok": true, "ignored": true }))
        }
        EventDecision::Reject => {
            emit_metric("rejected_policy");
            json_response(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "error": "invalid_event" }),
            )
        }
        EventDecision::Accept(event) => {
            let permit = match app.workflow_limit.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    emit_metric("rejected_capacity");
                    return json_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        json!({ "ok": false, "error": "capacity_exceeded" }),
                    );
                }
            };
            match app.store.claim(&event.event_id) {
                Ok(ClaimOutcome::Duplicate) => {
                    emit_metric("duplicate");
                    drop(permit);
                    json_response(StatusCode::OK, json!({ "ok": true, "duplicate": true }))
                }
                Err(_) => {
                    emit_metric("journal_failure");
                    drop(permit);
                    json_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        json!({ "ok": false, "error": "temporarily_unavailable" }),
                    )
                }
                Ok(ClaimOutcome::Claimed) => {
                    emit_metric("accepted");
                    let span = info_span!(
                        "slack_dual_model_workflow",
                        slack_event_id = %event.event_id,
                        slack_team_id = %event.team_id,
                        slack_channel_id = %event.channel,
                        slack_thread_ts = %event.thread_ts
                    );
                    tokio::spawn(process_event(app, event, permit).instrument(span));
                    json_response(StatusCode::OK, json!({ "ok": true, "accepted": true }))
                }
            }
        }
    }
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn emit_metric(outcome: &'static str) {
    info!(
        target: "fiducia.slack_bridge.metrics",
        metric = "slack_bridge_requests_total",
        outcome,
        value = 1_u64
    );
}

async fn process_event(app: Arc<SlackApp>, event: AcceptedEvent, _permit: OwnedSemaphorePermit) {
    if app.config.dry_run {
        if app.store.complete(&event.event_id).is_err() {
            warn!("failed to persist dry-run completion");
        }
        emit_metric("dry_run");
        return;
    }

    let workflow_id = match app.create_workflow(&event).await {
        Ok(workflow_id) => {
            if app
                .store
                .set_workflow(&event.event_id, &workflow_id)
                .is_err()
            {
                warn!("failed to persist workflow correlation");
                post_start_failure_pair(&app, &event).await;
                emit_metric("failed");
                return;
            }
            workflow_id
        }
        Err(_) => {
            post_start_failure_pair(&app, &event).await;
            emit_metric("failed");
            return;
        }
    };

    let deadline = Instant::now() + app.config.workflow_timeout;
    let mut posted = app
        .store
        .snapshot(&event.event_id)
        .map(|record| record.posted_agents)
        .unwrap_or_default();
    let mut terminal = false;

    while Instant::now() < deadline {
        if let Ok(workflow) = app.get_workflow(&workflow_id).await {
            terminal = workflow.status.stage == "completed";
            for model in app.config.models() {
                if posted.contains(model.agent_key) {
                    continue;
                }
                if let Some(submission) = workflow
                    .submissions
                    .iter()
                    .find(|submission| submission.agent_key == model.agent_key)
                {
                    let text = labeled_reply(model, &submission.content);
                    if app
                        .post_slack_reply(&event.channel, &event.thread_ts, &text)
                        .await
                        .is_ok()
                    {
                        posted.insert(model.agent_key.to_string());
                        if app
                            .store
                            .mark_posted(&event.event_id, model.agent_key)
                            .is_err()
                        {
                            warn!("failed to persist Slack reply marker");
                            emit_metric("journal_failure");
                            return;
                        }
                    }
                }
            }
            if posted.len() == 2 {
                if app.store.complete(&event.event_id).is_err() {
                    warn!("failed to persist workflow completion");
                }
                emit_metric("succeeded");
                return;
            }
            if terminal {
                break;
            }
        }
        sleep(app.config.poll_interval).await;
    }

    for model in app.config.models() {
        if posted.contains(model.agent_key) {
            continue;
        }
        let reason = if terminal {
            "The bridge completed without a submission from this configured model."
        } else {
            "No response was available before the bounded workflow deadline."
        };
        let text = labeled_failure(model, reason);
        if app
            .post_slack_reply(&event.channel, &event.thread_ts, &text)
            .await
            .is_ok()
        {
            if app
                .store
                .mark_posted(&event.event_id, model.agent_key)
                .is_err()
            {
                warn!("failed to persist partial-failure reply marker");
                emit_metric("journal_failure");
                return;
            }
        }
    }

    let final_record = app.store.snapshot(&event.event_id);
    if final_record
        .as_ref()
        .is_some_and(|record| record.posted_agents.len() == 2)
    {
        if app.store.complete(&event.event_id).is_err() {
            warn!("failed to persist partial workflow completion");
        }
        if posted.is_empty() {
            emit_metric("failed");
        } else {
            emit_metric("partial");
        }
    } else {
        emit_metric("reply_failed");
    }
}

async fn post_start_failure_pair(app: &SlackApp, event: &AcceptedEvent) {
    let mut posted = 0usize;
    for model in app.config.models() {
        let text = labeled_failure(
            model,
            "The bounded bridge workflow could not be started safely.",
        );
        if app
            .post_slack_reply(&event.channel, &event.thread_ts, &text)
            .await
            .is_ok()
        {
            posted += 1;
            if app
                .store
                .mark_posted(&event.event_id, model.agent_key)
                .is_err()
            {
                warn!("failed to persist startup-failure reply marker");
                return;
            }
        }
    }
    if posted == 2 && app.store.complete(&event.event_id).is_err() {
        warn!("failed to persist startup-failure completion");
    }
}

fn labeled_reply(model: ModelRoute<'_>, content: &str) -> String {
    format!(
        "*{} (`{}`)*\n{}",
        model.label,
        model.agent_key,
        truncate_utf8(content, MAX_SLACK_MESSAGE_BYTES)
    )
}

fn labeled_failure(model: ModelRoute<'_>, reason: &str) -> String {
    format!(
        "*{} (`{}`)*\n:warning: {}",
        model.label, model.agent_key, reason
    )
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_string();
    }
    const SUFFIX: &str = "\n… [truncated]";
    if maximum_bytes <= SUFFIX.len() {
        let mut boundary = maximum_bytes.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        return value[..boundary].to_string();
    }
    let content_limit = maximum_bytes - SUFFIX.len();
    let mut boundary = content_limit.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = value[..boundary].to_string();
    truncated.push_str(SUFFIX);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(path: PathBuf) -> SlackConfig {
        SlackConfig {
            host: DEFAULT_HOST.parse().unwrap(),
            port: DEFAULT_PORT,
            signing_secret: "test-signing-secret-which-is-long-enough".to_string(),
            bot_token: Some("xoxb-test".to_string()),
            bot_user_id: Some("UBOT".to_string()),
            allowed_team_ids: ["T1".to_string()].into_iter().collect(),
            allowed_channel_ids: ["C1".to_string()].into_iter().collect(),
            allowed_thread_ts: BTreeSet::new(),
            command_prefix: DEFAULT_COMMAND_PREFIX.to_string(),
            bridge_url: DEFAULT_BRIDGE_URL.to_string(),
            bridge_bearer: None,
            slack_post_message_url: SLACK_POST_MESSAGE_URL.to_string(),
            claude_agent_key: DEFAULT_CLAUDE_AGENT_KEY.to_string(),
            openai_agent_key: DEFAULT_OPENAI_AGENT_KEY.to_string(),
            dry_run: false,
            idempotency_path: path,
            max_request_age_secs: DEFAULT_MAX_REQUEST_AGE_SECS,
            workflow_timeout: Duration::from_secs(DEFAULT_WORKFLOW_TIMEOUT_SECS),
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent_workflows: DEFAULT_MAX_CONCURRENT_WORKFLOWS,
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        env::temp_dir()
            .join(format!("fiducia-slack-{label}-{}", uuid::Uuid::new_v4()))
            .join("events.jsonl")
    }

    fn event_envelope(text: &str) -> SlackEnvelope {
        SlackEnvelope {
            kind: "event_callback".to_string(),
            challenge: None,
            event_id: Some("Ev123".to_string()),
            team_id: Some("T1".to_string()),
            event: Some(SlackEvent {
                kind: "app_mention".to_string(),
                channel: Some("C1".to_string()),
                user: Some("U1".to_string()),
                text: Some(text.to_string()),
                ts: Some("1234.5678".to_string()),
                thread_ts: Some("1000.0001".to_string()),
                bot_id: None,
                subtype: None,
            }),
        }
    }

    fn signed_headers(secret: &str, timestamp: i64, body: &[u8]) -> HeaderMap {
        let timestamp = timestamp.to_string();
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"v0:");
        mac.update(timestamp.as_bytes());
        mac.update(b":");
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let signature = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-request-timestamp", timestamp.parse().unwrap());
        headers.insert(
            "x-slack-signature",
            format!("v0={signature}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn valid_signature_is_accepted() {
        let config = test_config(temp_path("signature-valid"));
        let body = br#"{"type":"event_callback"}"#;
        let now = 1_700_000_000;
        let headers = signed_headers(&config.signing_secret, now, body);
        assert_eq!(verify_slack_signature(&config, &headers, body, now), Ok(()));
    }

    #[test]
    fn wrong_signature_is_rejected() {
        let config = test_config(temp_path("signature-wrong"));
        let body = br#"{"type":"event_callback"}"#;
        let now = 1_700_000_000;
        let headers = signed_headers("different-secret", now, body);
        assert_eq!(
            verify_slack_signature(&config, &headers, body, now),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn stale_signature_is_rejected() {
        let config = test_config(temp_path("signature-stale"));
        let body = b"{}";
        let now = 1_700_000_000;
        let headers = signed_headers(&config.signing_secret, now - 301, body);
        assert_eq!(
            verify_slack_signature(&config, &headers, body, now),
            Err(SignatureError::Stale)
        );
    }

    #[test]
    fn far_future_signature_is_rejected() {
        let config = test_config(temp_path("signature-future"));
        let body = b"{}";
        let now = 1_700_000_000;
        let headers = signed_headers(&config.signing_secret, now + 301, body);
        assert_eq!(
            verify_slack_signature(&config, &headers, body, now),
            Err(SignatureError::Stale)
        );
    }

    #[test]
    fn malformed_signature_hex_is_rejected() {
        let config = test_config(temp_path("signature-hex"));
        let body = b"{}";
        let now = 1_700_000_000;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            now.to_string().parse().unwrap(),
        );
        headers.insert("x-slack-signature", "v0=not-hex".parse().unwrap());
        assert_eq!(
            verify_slack_signature(&config, &headers, body, now),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn missing_signature_headers_are_rejected() {
        let config = test_config(temp_path("signature-missing"));
        assert_eq!(
            verify_slack_signature(&config, &HeaderMap::new(), b"{}", 1),
            Err(SignatureError::Missing)
        );
    }

    #[test]
    fn url_verification_is_separate_from_command_events() {
        let config = test_config(temp_path("challenge"));
        let envelope = SlackEnvelope {
            kind: "url_verification".to_string(),
            challenge: Some("abc123".to_string()),
            event_id: None,
            team_id: Some("T1".to_string()),
            event: None,
        };
        assert_eq!(
            classify_event(&config, envelope),
            EventDecision::Challenge("abc123".to_string())
        );
    }

    #[test]
    fn url_verification_from_unapproved_workspace_is_ignored() {
        let config = test_config(temp_path("challenge-denied"));
        let envelope = SlackEnvelope {
            kind: "url_verification".to_string(),
            challenge: Some("abc123".to_string()),
            event_id: None,
            team_id: Some("T2".to_string()),
            event: None,
        };
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn bot_events_are_ignored() {
        let config = test_config(temp_path("bot"));
        let mut envelope = event_envelope("!ask-both hello");
        envelope.event.as_mut().unwrap().bot_id = Some("B1".to_string());
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn self_events_are_ignored() {
        let config = test_config(temp_path("self"));
        let mut envelope = event_envelope("!ask-both hello");
        envelope.event.as_mut().unwrap().user = Some("UBOT".to_string());
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn message_subtypes_are_ignored_to_prevent_loops() {
        let config = test_config(temp_path("subtype"));
        let mut envelope = event_envelope("!ask-both hello");
        envelope.event.as_mut().unwrap().subtype = Some("bot_message".to_string());
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn unapproved_channel_is_ignored() {
        let config = test_config(temp_path("channel"));
        let mut envelope = event_envelope("!ask-both hello");
        envelope.event.as_mut().unwrap().channel = Some("C2".to_string());
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn optional_thread_allowlist_is_enforced() {
        let mut config = test_config(temp_path("thread"));
        config.allowed_thread_ts.insert("2000.0001".to_string());
        let envelope = event_envelope("!ask-both hello");
        assert_eq!(classify_event(&config, envelope), EventDecision::Ignore);
    }

    #[test]
    fn non_command_messages_are_ignored() {
        let config = test_config(temp_path("non-command"));
        assert_eq!(
            classify_event(&config, event_envelope("hello")),
            EventDecision::Ignore
        );
    }

    #[test]
    fn leading_app_mention_is_removed_before_command_parsing() {
        assert_eq!(
            parse_command("<@UBOT> !ask-both explain raft", "!ask-both"),
            CommandParse::Prompt("explain raft".to_string())
        );
    }

    #[test]
    fn prefix_must_end_at_a_token_boundary() {
        assert_eq!(
            parse_command("!ask-both-now hello", "!ask-both"),
            CommandParse::NotCommand
        );
    }

    #[test]
    fn empty_command_is_rejected() {
        assert_eq!(
            parse_command("!ask-both   ", "!ask-both"),
            CommandParse::Invalid
        );
    }

    #[test]
    fn root_message_uses_its_timestamp_as_thread() {
        let config = test_config(temp_path("root-thread"));
        let mut envelope = event_envelope("!ask-both hello");
        envelope.event.as_mut().unwrap().thread_ts = None;
        match classify_event(&config, envelope) {
            EventDecision::Accept(event) => assert_eq!(event.thread_ts, "1234.5678"),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn existing_thread_timestamp_is_preserved() {
        let config = test_config(temp_path("existing-thread"));
        match classify_event(&config, event_envelope("!ask-both hello")) {
            EventDecision::Accept(event) => assert_eq!(event.thread_ts, "1000.0001"),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn workflow_payload_routes_exactly_two_requested_agents() {
        let config = test_config(temp_path("payload"));
        let event = match classify_event(&config, event_envelope("!ask-both hello")) {
            EventDecision::Accept(event) => event,
            other => panic!("unexpected decision: {other:?}"),
        };
        let payload = workflow_create_payload(&config, &event);
        assert_eq!(payload["mode"], "competitive");
        assert_eq!(payload["worker_count"], 2);
        assert_eq!(
            payload["agent_keys"],
            json!([DEFAULT_CLAUDE_AGENT_KEY, DEFAULT_OPENAI_AGENT_KEY])
        );
    }

    #[test]
    fn workflow_title_does_not_copy_the_prompt() {
        let config = test_config(temp_path("payload-title"));
        let event = AcceptedEvent {
            event_id: "Ev123".to_string(),
            team_id: "T1".to_string(),
            channel: "C1".to_string(),
            thread_ts: "1.1".to_string(),
            user: "U1".to_string(),
            prompt: "sensitive prompt".to_string(),
        };
        let payload = workflow_create_payload(&config, &event);
        assert!(!payload["title"].as_str().unwrap().contains("sensitive"));
    }

    #[test]
    fn first_event_claim_wins_and_retry_is_duplicate() {
        let path = temp_path("claim");
        let store = EventStore::open(path.clone()).unwrap();
        assert_eq!(store.claim("Ev123").unwrap(), ClaimOutcome::Claimed);
        assert_eq!(store.claim("Ev123").unwrap(), ClaimOutcome::Duplicate);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn event_claim_survives_restart() {
        let path = temp_path("restart");
        {
            let store = EventStore::open(path.clone()).unwrap();
            assert_eq!(store.claim("Ev123").unwrap(), ClaimOutcome::Claimed);
        }
        let reopened = EventStore::open(path.clone()).unwrap();
        assert_eq!(reopened.claim("Ev123").unwrap(), ClaimOutcome::Duplicate);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn workflow_and_reply_markers_are_persisted() {
        let path = temp_path("markers");
        {
            let store = EventStore::open(path.clone()).unwrap();
            store.claim("Ev123").unwrap();
            store.set_workflow("Ev123", "wf-123").unwrap();
            store
                .mark_posted("Ev123", DEFAULT_CLAUDE_AGENT_KEY)
                .unwrap();
            store.complete("Ev123").unwrap();
        }
        let reopened = EventStore::open(path.clone()).unwrap();
        let record = reopened.snapshot("Ev123").unwrap();
        assert_eq!(record.state, EventState::Completed);
        assert_eq!(record.workflow_id.as_deref(), Some("wf-123"));
        assert!(record.posted_agents.contains(DEFAULT_CLAUDE_AGENT_KEY));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_journal_fails_closed() {
        let path = temp_path("corrupt");
        prepare_private_parent(&path).unwrap();
        fs::write(&path, b"not-json\n").unwrap();
        assert!(matches!(
            EventStore::open(path.clone()),
            Err(AdapterError::Journal)
        ));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn labels_are_deterministic() {
        let model = ModelRoute {
            agent_key: DEFAULT_CLAUDE_AGENT_KEY,
            label: "Claude Fable",
        };
        assert_eq!(
            labeled_reply(model, "answer"),
            "*Claude Fable (`claude-fable-5`)*\nanswer"
        );
    }

    #[test]
    fn utf8_truncation_preserves_character_boundaries() {
        let value = "é".repeat(100);
        let truncated = truncate_utf8(&value, 31);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.len() <= 31);
        assert!(truncated.ends_with("[truncated]"));
    }

    #[test]
    fn loopback_http_bridge_url_is_allowed() {
        assert!(normalize_bridge_url("http://127.0.0.1:8142").is_ok());
        assert!(normalize_bridge_url("http://localhost:8142").is_ok());
    }

    #[test]
    fn remote_http_bridge_url_is_rejected() {
        assert!(normalize_bridge_url("http://bridge.example.com").is_err());
    }

    #[test]
    fn remote_https_bridge_url_is_allowed() {
        assert!(normalize_bridge_url("https://bridge.example.com").is_ok());
    }

    #[test]
    fn event_ids_are_strictly_bounded() {
        assert!(valid_event_id("Ev_123-abc:1.2"));
        assert!(!valid_event_id("../bad"));
        assert!(!valid_event_id(&"a".repeat(MAX_EVENT_ID_BYTES + 1)));
    }

    #[test]
    fn slack_timestamp_shape_is_validated() {
        assert!(valid_slack_timestamp("1234567890.123456"));
        assert!(!valid_slack_timestamp("1234567890"));
        assert!(!valid_slack_timestamp("abc.123"));
    }

    #[derive(Clone)]
    struct MockBridgeState {
        creates: Arc<Mutex<Vec<Value>>>,
        submissions: Arc<Vec<Value>>,
    }

    #[derive(Clone, Default)]
    struct MockSlackState {
        messages: Arc<Mutex<Vec<Value>>>,
    }

    async fn mock_create_workflow(
        State(state): State<MockBridgeState>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        state.creates.lock().push(payload);
        Json(mock_workflow_response("running", &[]))
    }

    async fn mock_get_workflow(State(state): State<MockBridgeState>) -> Json<Value> {
        Json(mock_workflow_response("completed", &state.submissions))
    }

    async fn mock_post_message(
        State(state): State<MockSlackState>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        state.messages.lock().push(payload);
        Json(json!({ "ok": true }))
    }

    fn mock_workflow_response(stage: &str, submissions: &[Value]) -> Value {
        json!({
            "workflow": {
                "plan": {
                    "id": "wf-123",
                    "assignments": [
                        { "agent_key": DEFAULT_CLAUDE_AGENT_KEY },
                        { "agent_key": DEFAULT_OPENAI_AGENT_KEY }
                    ]
                },
                "status": { "stage": stage },
                "submissions": submissions
            }
        })
    }

    async fn spawn_test_server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/"), handle)
    }

    async fn send_signed_event(
        base_url: &str,
        config: &SlackConfig,
        body: Vec<u8>,
    ) -> reqwest::Response {
        let now = Utc::now().timestamp();
        Client::new()
            .post(format!("{base_url}slack/events"))
            .headers(signed_headers(&config.signing_secret, now, &body))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn wait_for_messages(messages: &Arc<Mutex<Vec<Value>>>, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if messages.lock().len() >= count {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn signed_event_routes_once_and_posts_two_replies_to_the_origin_thread() {
        let bridge_state = MockBridgeState {
            creates: Arc::new(Mutex::new(Vec::new())),
            submissions: Arc::new(vec![
                json!({ "agent_key": DEFAULT_CLAUDE_AGENT_KEY, "content": "claude answer" }),
                json!({ "agent_key": DEFAULT_OPENAI_AGENT_KEY, "content": "openai answer" }),
            ]),
        };
        let bridge_router = Router::new()
            .route("/workflows", post(mock_create_workflow))
            .route("/workflows/{workflow_id}", get(mock_get_workflow))
            .with_state(bridge_state.clone());
        let (bridge_url, bridge_handle) = spawn_test_server(bridge_router).await;

        let slack_state = MockSlackState::default();
        let slack_router = Router::new()
            .route("/api/chat.postMessage", post(mock_post_message))
            .with_state(slack_state.clone());
        let (slack_url, slack_handle) = spawn_test_server(slack_router).await;

        let path = temp_path("integration-success");
        let mut config = test_config(path.clone());
        config.bridge_url = bridge_url;
        config.slack_post_message_url = format!("{slack_url}api/chat.postMessage");
        config.poll_interval = Duration::from_millis(10);
        config.workflow_timeout = Duration::from_secs(1);
        let app = Arc::new(SlackApp::new(config.clone()).unwrap());
        let (adapter_url, adapter_handle) = spawn_test_server(router(app)).await;
        let body = serde_json::to_vec(&event_envelope("!ask-both explain raft")).unwrap();

        let first = send_signed_event(&adapter_url, &config, body.clone()).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = send_signed_event(&adapter_url, &config, body).await;
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = second.json::<Value>().await.unwrap();
        assert_eq!(second_body["duplicate"], true);

        wait_for_messages(&slack_state.messages, 2).await;
        assert_eq!(bridge_state.creates.lock().len(), 1);
        let payload = bridge_state.creates.lock()[0].clone();
        assert_eq!(
            payload["agent_keys"],
            json!([DEFAULT_CLAUDE_AGENT_KEY, DEFAULT_OPENAI_AGENT_KEY])
        );
        assert_eq!(payload["worker_count"], 2);

        let messages = slack_state.messages.lock().clone();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| message["channel"] == "C1"));
        assert!(messages
            .iter()
            .all(|message| message["thread_ts"] == "1000.0001"));
        assert!(messages.iter().any(|message| message["text"]
            .as_str()
            .is_some_and(|text| text.contains("Claude Fable") && text.contains("claude answer"))));
        assert!(messages
            .iter()
            .any(|message| message["text"].as_str().is_some_and(|text| text
                .contains("ChatGPT 5.6 Sol")
                && text.contains("openai answer"))));

        adapter_handle.abort();
        slack_handle.abort();
        bridge_handle.abort();
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn completed_one_provider_workflow_preserves_success_and_labels_failure() {
        let bridge_state = MockBridgeState {
            creates: Arc::new(Mutex::new(Vec::new())),
            submissions: Arc::new(vec![
                json!({ "agent_key": DEFAULT_CLAUDE_AGENT_KEY, "content": "claude answer" }),
            ]),
        };
        let bridge_router = Router::new()
            .route("/workflows", post(mock_create_workflow))
            .route("/workflows/{workflow_id}", get(mock_get_workflow))
            .with_state(bridge_state);
        let (bridge_url, bridge_handle) = spawn_test_server(bridge_router).await;

        let slack_state = MockSlackState::default();
        let slack_router = Router::new()
            .route("/api/chat.postMessage", post(mock_post_message))
            .with_state(slack_state.clone());
        let (slack_url, slack_handle) = spawn_test_server(slack_router).await;

        let path = temp_path("integration-partial");
        let mut config = test_config(path.clone());
        config.bridge_url = bridge_url;
        config.slack_post_message_url = format!("{slack_url}api/chat.postMessage");
        config.poll_interval = Duration::from_millis(10);
        config.workflow_timeout = Duration::from_secs(1);
        let app = Arc::new(SlackApp::new(config.clone()).unwrap());
        let (adapter_url, adapter_handle) = spawn_test_server(router(app)).await;
        let body = serde_json::to_vec(&event_envelope("!ask-both explain raft")).unwrap();

        let response = send_signed_event(&adapter_url, &config, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        wait_for_messages(&slack_state.messages, 2).await;

        let messages = slack_state.messages.lock().clone();
        assert!(messages.iter().any(|message| message["text"]
            .as_str()
            .is_some_and(|text| text.contains("claude answer"))));
        assert!(messages.iter().any(|message| message["text"]
            .as_str()
            .is_some_and(|text| text.contains("ChatGPT 5.6 Sol") && text.contains(":warning:"))));

        adapter_handle.abort();
        slack_handle.abort();
        bridge_handle.abort();
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn dry_run_acknowledges_without_calling_bridge_or_slack() {
        let bridge_state = MockBridgeState {
            creates: Arc::new(Mutex::new(Vec::new())),
            submissions: Arc::new(Vec::new()),
        };
        let bridge_router = Router::new()
            .route("/workflows", post(mock_create_workflow))
            .route("/workflows/{workflow_id}", get(mock_get_workflow))
            .with_state(bridge_state.clone());
        let (bridge_url, bridge_handle) = spawn_test_server(bridge_router).await;

        let slack_state = MockSlackState::default();
        let slack_router = Router::new()
            .route("/api/chat.postMessage", post(mock_post_message))
            .with_state(slack_state.clone());
        let (slack_url, slack_handle) = spawn_test_server(slack_router).await;

        let path = temp_path("integration-dry-run");
        let mut config = test_config(path.clone());
        config.dry_run = true;
        config.bot_token = None;
        config.bridge_url = bridge_url;
        config.slack_post_message_url = format!("{slack_url}api/chat.postMessage");
        let app = Arc::new(SlackApp::new(config.clone()).unwrap());
        let store = app.store.path.clone();
        let (adapter_url, adapter_handle) = spawn_test_server(router(app)).await;
        let body = serde_json::to_vec(&event_envelope("!ask-both explain raft")).unwrap();

        let response = send_signed_event(&adapter_url, &config, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let file = fs::read_to_string(&store).unwrap_or_default();
                if file.contains("completed") {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(bridge_state.creates.lock().is_empty());
        assert!(slack_state.messages.lock().is_empty());

        adapter_handle.abort();
        slack_handle.abort();
        bridge_handle.abort();
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
