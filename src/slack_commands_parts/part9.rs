const SLACK_ACK_DEADLINE: Duration = Duration::from_millis(2_500);
const EXPECTED_APP_ID_ENV: &str = "SLACK_EXPECTED_APP_ID";
const EXPECTED_TEAM_ID_ENV: &str = "SLACK_EXPECTED_TEAM_ID";
const INSTALLED_APP_ID: &str = "A0BMBAMM5NJ";
const INSTALLED_TEAM_ID: &str = "T01B3C83PMK";

fn configured_slack_identity(config: &Config) -> Result<Option<(String, String)>> {
    let app_id = env_opt(EXPECTED_APP_ID_ENV);
    let team_id = env_opt(EXPECTED_TEAM_ID_ENV);
    match (app_id, team_id) {
        (Some(app_id), Some(team_id)) => Ok(Some((
            identifier(EXPECTED_APP_ID_ENV, &app_id)?,
            identifier(EXPECTED_TEAM_ID_ENV, &team_id)?,
        ))),
        (None, None) if config.host.is_loopback() => Ok(None),
        (None, None) => Err(Error::Config(format!(
            "{EXPECTED_APP_ID_ENV} and {EXPECTED_TEAM_ID_ENV} are required for non-loopback binds"
        ))),
        _ => Err(Error::Config(format!(
            "{EXPECTED_APP_ID_ENV} and {EXPECTED_TEAM_ID_ENV} must be configured together"
        ))),
    }
}

fn validate_slash_envelope(
    config: &Config,
    body: &[u8],
    expected_provider: Provider,
) -> Result<()> {
    let form = parse_form(body)?;
    let actual_provider = Provider::from_command(&field(&form, "command")?).ok_or(Error::Request)?;
    if actual_provider != expected_provider {
        return Err(Error::Request);
    }
    if let Some((expected_app_id, expected_team_id)) = configured_slack_identity(config)? {
        if id_field(&form, "api_app_id")? != expected_app_id
            || id_field(&form, "team_id")? != expected_team_id
        {
            return Err(Error::Policy);
        }
    }
    Ok(())
}

fn parse_interaction_envelope(config: &Config, body: &[u8]) -> Result<InteractionPayload> {
    let form = parse_form(body)?;
    let payload = field(&form, "payload")?;
    let value = serde_json::from_str::<Value>(&payload).map_err(|_| Error::Request)?;
    if let Some((expected_app_id, expected_team_id)) = configured_slack_identity(config)? {
        let app_matches = value
            .get("api_app_id")
            .and_then(Value::as_str)
            .is_some_and(|value| value == expected_app_id);
        let team_matches = value
            .get("team")
            .and_then(|team| team.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|value| value == expected_team_id);
        if !app_matches || !team_matches {
            return Err(Error::Policy);
        }
    }
    serde_json::from_value::<InteractionPayload>(value).map_err(|_| Error::Request)
}

#[cfg(test)]
mod installed_app_contract_tests {
    use super::*;

    fn loopback_config() -> Config {
        Config {
            host: "127.0.0.1".parse().unwrap(),
            port: 8151,
            signing_secret: "test-signing-secret".into(),
            bot_token: "test-bot-token".into(),
            registry_path: PathBuf::from("/tmp/registry.json"),
            state_dir: PathBuf::from("/tmp/slack-command-state"),
            bridge_url: "http://127.0.0.1:8142/".into(),
            bridge_bearer: None,
            coordinator_url: "http://127.0.0.1:8160/".into(),
            coordinator_bearer: None,
            slack_api_base_url: "http://127.0.0.1:8170/api/".into(),
            claude_agent: "claude-fable-5".into(),
            chatgpt_agent: "gpt-5.6-sol".into(),
            linear_run_project_id: DEFAULT_LINEAR_RUN_PROJECT.into(),
            context_messages: 5,
            dry_run: true,
            max_concurrent_runs: 1,
        }
    }

    #[test]
    fn exact_endpoint_provider_is_enforced_even_for_loopback_tests() {
        let config = loopback_config();
        let body = b"command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=test&trigger_id=1";
        assert!(validate_slash_envelope(&config, body, Provider::Chatgpt).is_ok());
        assert!(validate_slash_envelope(&config, body, Provider::Claude).is_err());
    }

    #[test]
    fn reviewed_manifest_keeps_exact_app_and_routes() {
        let manifest = include_str!("../../slack-app/manifest.yaml");
        assert!(manifest.contains("name: alex-main-agent"));
        assert!(manifest.contains("command: /ores-claude"));
        assert!(manifest.contains("command: /ores-chatgpt"));
        assert!(manifest.contains("https://api.fiducia.cloud/slack/commands/ores-claude"));
        assert!(manifest.contains("https://api.fiducia.cloud/slack/commands/ores-chatgpt"));
        assert!(manifest.contains("https://api.fiducia.cloud/slack/interactions"));
        assert!(manifest.contains("token_rotation_enabled: true"));
        assert!(!manifest.contains("xoxb-"));
        assert!(!manifest.contains("signing_secret"));
        assert_eq!(INSTALLED_APP_ID, "A0BMBAMM5NJ");
        assert_eq!(INSTALLED_TEAM_ID, "T01B3C83PMK");
    }
}
