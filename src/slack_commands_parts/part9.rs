const SLACK_ACK_DEADLINE: Duration = Duration::from_millis(2_500);
const EXPECTED_APP_ID_ENV: &str = "SLACK_EXPECTED_APP_ID";
const EXPECTED_TEAM_ID_ENV: &str = "SLACK_EXPECTED_TEAM_ID";
const ALLOW_UNPINNED_IDENTITY_ENV: &str = "SLACK_ALLOW_UNPINNED_IDENTITY";

fn configured_slack_identity_from_values(
    config: &Config,
    app_id: Option<String>,
    team_id: Option<String>,
) -> Result<Option<(String, String)>> {
    match (app_id, team_id) {
        (Some(app_id), Some(team_id)) => Ok(Some((
            identifier(EXPECTED_APP_ID_ENV, &app_id)?,
            identifier(EXPECTED_TEAM_ID_ENV, &team_id)?,
        ))),
        // The bind address is not a proxy for exposure. A same-host TLS
        // terminator forwarding to 127.0.0.1 is the reviewed internet-facing
        // shape, so a loopback bind must not silently disable app/team pinning.
        (None, None) if config.allow_unpinned_identity => Ok(None),
        (None, None) => Err(Error::Config(format!(
            "{EXPECTED_APP_ID_ENV} and {EXPECTED_TEAM_ID_ENV} are required unless {ALLOW_UNPINNED_IDENTITY_ENV} is set"
        ))),
        _ => Err(Error::Config(format!(
            "{EXPECTED_APP_ID_ENV} and {EXPECTED_TEAM_ID_ENV} must be configured together"
        ))),
    }
}

fn configured_slack_identity(config: &Config) -> Result<Option<(String, String)>> {
    configured_slack_identity_from_values(
        config,
        env_opt(EXPECTED_APP_ID_ENV),
        env_opt(EXPECTED_TEAM_ID_ENV),
    )
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
    interaction_payload(config, value)
}

/// Identity-pin and decode one interaction payload.
///
/// Split out of `parse_interaction_envelope` because a Socket Mode frame
/// carries the payload as JSON directly rather than form-wrapped under
/// `payload=`. Re-encoding it to a form just to parse it back would be a second
/// path through the decoder; both transports share this one instead.
fn interaction_payload(config: &Config, value: Value) -> Result<InteractionPayload> {
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

    const INSTALLED_APP_ID: &str = "A0BMBAMM5NJ";
    const INSTALLED_TEAM_ID: &str = "T01B3C83PMK";

    fn unpinned_config() -> Config {
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
            socket_mode: false,
            app_token: None,
            dry_run: true,
            max_concurrent_runs: 1,
            allow_unpinned_identity: true,
        }
    }

    fn pinned_config() -> Config {
        Config {
            allow_unpinned_identity: false,
            ..unpinned_config()
        }
    }

    fn config_error(result: Result<Option<(String, String)>>) -> String {
        match result {
            Err(Error::Config(message)) => message,
            other => panic!("expected configuration error, got {other:?}"),
        }
    }

    #[test]
    fn identity_pinning_may_be_waived_only_by_explicit_opt_in() {
        assert_eq!(
            configured_slack_identity_from_values(&unpinned_config(), None, None).unwrap(),
            None,
        );
    }

    #[test]
    fn a_loopback_bind_alone_does_not_waive_identity_pinning() {
        // Regression guard: the reviewed production deployment terminates TLS
        // in a same-host proxy and forwards to 127.0.0.1. Inferring "isolated"
        // from that bind address silently disabled app/team pinning in exactly
        // the deployment that most needed it.
        let internet_facing_behind_a_proxy = Config {
            host: "127.0.0.1".parse().unwrap(),
            allow_unpinned_identity: false,
            ..unpinned_config()
        };
        assert!(internet_facing_behind_a_proxy.host.is_loopback());
        assert_eq!(
            config_error(configured_slack_identity_from_values(
                &internet_facing_behind_a_proxy,
                None,
                None,
            )),
            "SLACK_EXPECTED_APP_ID and SLACK_EXPECTED_TEAM_ID are required unless \
             SLACK_ALLOW_UNPINNED_IDENTITY is set",
        );
    }

    #[test]
    fn a_pinned_deployment_requires_both_installed_app_identifiers() {
        assert_eq!(
            config_error(configured_slack_identity_from_values(
                &pinned_config(),
                None,
                None,
            )),
            "SLACK_EXPECTED_APP_ID and SLACK_EXPECTED_TEAM_ID are required unless \
             SLACK_ALLOW_UNPINNED_IDENTITY is set",
        );
    }

    #[test]
    fn partial_installed_app_identity_is_rejected_in_every_posture() {
        for config in [unpinned_config(), pinned_config()] {
            assert_eq!(
                config_error(configured_slack_identity_from_values(
                    &config,
                    Some(INSTALLED_APP_ID.into()),
                    None,
                )),
                "SLACK_EXPECTED_APP_ID and SLACK_EXPECTED_TEAM_ID must be configured together",
            );
            assert_eq!(
                config_error(configured_slack_identity_from_values(
                    &config,
                    None,
                    Some(INSTALLED_TEAM_ID.into()),
                )),
                "SLACK_EXPECTED_APP_ID and SLACK_EXPECTED_TEAM_ID must be configured together",
            );
        }
    }

    #[test]
    fn paired_installed_app_identity_is_validated_and_preserved() {
        let identity = configured_slack_identity_from_values(
            &pinned_config(),
            Some(INSTALLED_APP_ID.into()),
            Some(INSTALLED_TEAM_ID.into()),
        )
        .unwrap();
        assert_eq!(
            identity,
            Some((INSTALLED_APP_ID.into(), INSTALLED_TEAM_ID.into())),
        );

        assert!(configured_slack_identity_from_values(
            &pinned_config(),
            Some("invalid app id".into()),
            Some(INSTALLED_TEAM_ID.into()),
        )
        .is_err());
        assert!(configured_slack_identity_from_values(
            &pinned_config(),
            Some(INSTALLED_APP_ID.into()),
            Some("invalid team id".into()),
        )
        .is_err());
    }

    #[test]
    fn exact_endpoint_provider_is_enforced_even_when_identity_is_unpinned() {
        let config = unpinned_config();
        let body = b"command=%2Fx-ores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=test&trigger_id=1";
        assert!(validate_slash_envelope(&config, body, Provider::Chatgpt).is_ok());
        assert!(validate_slash_envelope(&config, body, Provider::Claude).is_err());
    }

    #[test]
    fn the_reviewed_namespace_passes_the_canonical_provider_envelope() {
        let config = unpinned_config();
        for (command, ok, not_ok) in [
            ("%2Fx-ores-claude", Provider::Claude, Provider::Chatgpt),
            ("%2Fx-ores-chatgpt", Provider::Chatgpt, Provider::Claude),
        ] {
            let body = format!(
                "command={command}&team_id=T1&channel_id=C1&user_id=U1&text=test&trigger_id=1"
            );
            assert!(validate_slash_envelope(&config, body.as_bytes(), ok).is_ok());
            assert!(validate_slash_envelope(&config, body.as_bytes(), not_ok).is_err());
        }
    }

    #[test]
    fn retired_pre_namespace_commands_are_refused_at_the_envelope() {
        let config = unpinned_config();
        for command in [
            "%2Fores-claude",
            "%2Fores-chatgpt",
            "%2Fx-claude",
            "%2Fx-chatgpt",
            "%2Fmy-claude",
            "%2Fmy-chatgpt",
        ] {
            let body = format!(
                "command={command}&team_id=T1&channel_id=C1&user_id=U1&text=test&trigger_id=1"
            );
            assert!(validate_slash_envelope(&config, body.as_bytes(), Provider::Claude).is_err());
            assert!(validate_slash_envelope(&config, body.as_bytes(), Provider::Chatgpt).is_err());
        }
    }

    #[test]
    fn reviewed_manifest_keeps_exact_app_commands_and_routes() {
        let manifest = include_str!("../../slack-app/manifest.yaml");
        assert!(manifest.contains("name: alex-main-agent"));
        for command in ["/x-ores-claude", "/x-ores-chatgpt"] {
            assert!(manifest.contains(&format!("command: {command}")));
        }
        // One command, one request URL. Slack routes by path, and the service
        // recovers the provider from that path, so a URL reused by a second
        // command would silently cross providers.
        assert_eq!(
            manifest
                .matches("https://api.fiducia.cloud/slack/commands/x-ores-claude")
                .count(),
            1
        );
        assert_eq!(
            manifest
                .matches("https://api.fiducia.cloud/slack/commands/x-ores-chatgpt")
                .count(),
            1
        );
        assert_eq!(manifest.matches("- command: ").count(), 2);
        for retired in [
            "command: /ores-claude",
            "command: /ores-chatgpt",
            "command: /x-claude",
            "command: /x-chatgpt",
            "command: /my-claude",
            "command: /my-chatgpt",
        ] {
            assert!(
                !manifest.contains(retired),
                "{retired} was retired by the /x-ores-* namespace"
            );
        }
        assert!(manifest.contains("https://api.fiducia.cloud/slack/interactions"));
        assert!(manifest.contains("- commands"));
        assert!(manifest.contains("- chat:write"));
        assert!(manifest.contains("- channels:history"));
        assert!(manifest.contains("- groups:history"));
        assert!(manifest.contains("- usergroups:read"));
        assert!(manifest.contains("token_rotation_enabled: true"));
        assert!(!manifest.contains("xoxb-"));
        assert!(!manifest.contains("signing_secret"));
        assert_eq!(INSTALLED_APP_ID, "A0BMBAMM5NJ");
        assert_eq!(INSTALLED_TEAM_ID, "T01B3C83PMK");
    }
}
