const SLACK_INTERNAL_HTTP_HOSTS_ENV: &str = "SLACK_INTERNAL_HTTP_HOSTS";

fn identifier(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.' | ':')
        })
    {
        return Err(Error::Config(format!(
            "{name} contains an invalid identifier"
        )));
    }
    Ok(value.to_string())
}

fn service_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value).map_err(|_| Error::Config("invalid service URL".into()))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "service URL must not contain credentials or query data".into(),
        ));
    }
    let internal_http_hosts = env_opt(SLACK_INTERNAL_HTTP_HOSTS_ENV);
    crate::bridge_origin_policy::validate_service_transport(
        &url,
        internal_http_hosts.as_deref(),
        SLACK_INTERNAL_HTTP_HOSTS_ENV,
        "Slack service",
    )
    .map_err(Error::Config)?;
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url.to_string())
}

fn loopback_url(value: &str) -> Result<bool> {
    let url = Url::parse(value).map_err(|_| Error::Config("invalid service URL".into()))?;
    Ok(url_host_is_loopback(&url))
}

fn url_host_is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        host.parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    })
}

#[derive(Clone, Debug)]
struct SlashCommand {
    team_id: String,
    channel_id: String,
    user_id: String,
    command: String,
    text: String,
    trigger_id: String,
}

impl SlashCommand {
    fn parse(body: &[u8]) -> Result<Self> {
        let form = parse_form(body)?;
        let command = field(&form, "command")?;
        Provider::from_command(&command).ok_or(Error::Request)?;
        Ok(Self {
            team_id: id_field(&form, "team_id")?,
            channel_id: id_field(&form, "channel_id")?,
            user_id: id_field(&form, "user_id")?,
            command,
            text: form.get("text").cloned().unwrap_or_default(),
            trigger_id: field(&form, "trigger_id")?,
        })
    }

    fn provider(&self) -> Provider {
        Provider::from_command(&self.command).expect("command was validated")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModalMetadata {
    provider: Provider,
    team_id: String,
    channel_id: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct InteractionPayload {
    #[serde(rename = "type")]
    kind: String,
    team: Identity,
    user: Identity,
    view: InteractionView,
}

#[derive(Debug, Deserialize)]
struct Identity {
    id: String,
}

#[derive(Debug, Deserialize)]
struct InteractionView {
    id: String,
    callback_id: String,
    private_metadata: String,
    state: InteractionState,
}

#[derive(Debug, Deserialize)]
struct InteractionState {
    values: BTreeMap<String, BTreeMap<String, InteractionValue>>,
}

#[derive(Debug, Deserialize)]
struct InteractionValue {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    selected_option: Option<SelectedOption>,
}

#[derive(Debug, Deserialize)]
struct SelectedOption {
    value: String,
}

#[derive(Clone, Debug)]
struct RunRequest {
    run_id: String,
    source_key: String,
    occurred_at: String,
    provider: Provider,
    team_id: String,
    channel_id: String,
    user_id: String,
    prompt: String,
    action: String,
    repository: Option<String>,
    linear_issue: Option<String>,
    capability: RequestedCapability,
    context_messages: usize,
}

impl RunRequest {
    fn direct(command: &SlashCommand, context_messages: usize) -> Result<Self> {
        let prompt = prompt(&command.text)?;
        let source_key = format!(
            "slash:{}:{}:{}:{}",
            command.team_id, command.channel_id, command.user_id, command.trigger_id
        );
        Ok(Self {
            run_id: run_id(&source_key),
            source_key,
            occurred_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            provider: command.provider(),
            team_id: command.team_id.clone(),
            channel_id: command.channel_id.clone(),
            user_id: command.user_id.clone(),
            linear_issue: find_issue(&prompt),
            prompt,
            action: "implement".into(),
            repository: None,
            capability: RequestedCapability::RepositoryWrite,
            context_messages,
        })
    }

    fn interaction(payload: InteractionPayload) -> Result<Self> {
        if payload.kind != "view_submission" || payload.view.callback_id != CALLBACK_ID {
            return Err(Error::Request);
        }
        let metadata = serde_json::from_str::<ModalMetadata>(&payload.view.private_metadata)
            .map_err(|_| Error::Request)?;
        if metadata.team_id != payload.team.id || metadata.user_id != payload.user.id {
            return Err(Error::Request);
        }
        let prompt = prompt(&text_value(&payload.view.state, "task", "task")?)?;
        let capability = match selected(&payload.view.state, "write_scope", "write_scope")?.as_str()
        {
            "read_only" => RequestedCapability::ReadOnly,
            "linear_write" => RequestedCapability::LinearWrite,
            "draft_pull_request" => RequestedCapability::RepositoryWrite,
            _ => return Err(Error::Request),
        };
        let context_messages =
            selected(&payload.view.state, "context_messages", "context_messages")?
                .parse::<usize>()
                .ok()
                .filter(|value| [0, 5, 10, 20].contains(value))
                .ok_or(Error::Request)?;
        let source_key = format!(
            "view:{}:{}:{}:{}",
            metadata.team_id, metadata.channel_id, metadata.user_id, payload.view.id
        );
        Ok(Self {
            run_id: run_id(&source_key),
            source_key,
            occurred_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            provider: metadata.provider,
            team_id: metadata.team_id,
            channel_id: metadata.channel_id,
            user_id: metadata.user_id,
            prompt,
            action: selected(&payload.view.state, "action", "action")?,
            repository: Some(selected(&payload.view.state, "repository", "repository")?),
            linear_issue: optional_text(&payload.view.state, "issue", "issue")?,
            capability,
            context_messages,
        })
    }
}

fn state_value<'a>(
    state: &'a InteractionState,
    block: &str,
    action: &str,
) -> Result<&'a InteractionValue> {
    state
        .values
        .get(block)
        .and_then(|actions| actions.get(action))
        .ok_or(Error::Request)
}

fn text_value(state: &InteractionState, block: &str, action: &str) -> Result<String> {
    state_value(state, block, action)?
        .value
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or(Error::Request)
}

fn optional_text(state: &InteractionState, block: &str, action: &str) -> Result<Option<String>> {
    Ok(state_value(state, block, action)?
        .value
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn selected(state: &InteractionState, block: &str, action: &str) -> Result<String> {
    state_value(state, block, action)?
        .selected_option
        .as_ref()
        .map(|option| option.value.clone())
        .ok_or(Error::Request)
}

fn prompt(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_PROMPT_BYTES || value.contains('\0') {
        return Err(Error::Request);
    }
    Ok(value.to_string())
}

fn run_id(source_key: &str) -> String {
    let digest = Sha256::digest(source_key.as_bytes());
    let suffix = digest
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("ores-{suffix}")
}

fn find_issue(text: &str) -> Option<String> {
    text.split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .find_map(|token| {
            let (team, number) = token.split_once('-')?;
            if !(2..=10).contains(&team.len())
                || !team.chars().all(|character| character.is_ascii_uppercase())
                || number.is_empty()
                || !number.chars().all(|character| character.is_ascii_digit())
            {
                return None;
            }
            Some(token.to_string())
        })
}
