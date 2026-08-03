#[cfg(test)]
mod registry_file_boundary_tests {
    use std::{fs, path::PathBuf};

    use uuid::Uuid;

    use super::*;

    fn unique_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fiducia-slack-registry-boundary-{label}-{}",
            Uuid::new_v4()
        ))
    }

    fn config(registry_path: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".parse().expect("loopback IP"),
            port: 8151,
            signing_secret: "test-signing-secret".into(),
            bot_token: "test-bot-token".into(),
            registry_path,
            state_dir: unique_path("state"),
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

    fn config_error(result: Result<App>) -> String {
        match result {
            Err(Error::Config(message)) => message,
            Err(other) => panic!("expected configuration error, got {other}"),
            Ok(_) => panic!("expected registry validation to fail"),
        }
    }

    #[test]
    fn oversized_registry_is_rejected_before_json_parsing() {
        let path = unique_path("oversized.json");
        fs::write(&path, vec![b' '; (MAX_REGISTRY_BYTES + 1) as usize])
            .expect("write oversized registry fixture");

        let message = config_error(App::new(config(path.clone())));
        assert!(message.contains("maximum size"));

        fs::remove_file(path).expect("remove oversized registry fixture");
    }

    #[test]
    fn registry_path_must_resolve_to_a_regular_file() {
        let path = unique_path("directory");
        fs::create_dir_all(&path).expect("create registry directory fixture");

        let message = config_error(App::new(config(path.clone())));
        assert!(message.contains("regular file"));

        fs::remove_dir_all(path).expect("remove registry directory fixture");
    }
}

#[cfg(test)]
mod ingress_security_tests {
    use axum::http::HeaderValue;

    use super::*;

    fn test_config(signing_secret: &str) -> Config {
        Config {
            host: "127.0.0.1".parse().expect("loopback IP"),
            port: DEFAULT_PORT,
            signing_secret: signing_secret.to_string(),
            bot_token: "test-bot-token".to_string(),
            registry_path: PathBuf::from("/tmp/slack-registry-test.json"),
            state_dir: PathBuf::from("/tmp/slack-command-test-state"),
            bridge_url: DEFAULT_BRIDGE_URL.to_string(),
            bridge_bearer: None,
            coordinator_url: DEFAULT_COORDINATOR_URL.to_string(),
            coordinator_bearer: None,
            slack_api_base_url: DEFAULT_SLACK_API_BASE_URL.to_string(),
            claude_agent: DEFAULT_CLAUDE_AGENT.to_string(),
            chatgpt_agent: DEFAULT_CHATGPT_AGENT.to_string(),
            linear_run_project_id: DEFAULT_LINEAR_RUN_PROJECT.to_string(),
            context_messages: DEFAULT_CONTEXT_MESSAGES,
            dry_run: true,
            max_concurrent_runs: 1,
        }
    }

    fn signed_headers(secret: &str, timestamp: i64, body: &[u8]) -> HeaderMap {
        let timestamp = timestamp.to_string();
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(b"v0:");
        mac.update(timestamp.as_bytes());
        mac.update(b":");
        mac.update(body);
        let signature = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_str(&timestamp).expect("timestamp header"),
        );
        headers.insert(
            "x-slack-signature",
            HeaderValue::from_str(&format!("v0={signature}")).expect("signature header"),
        );
        headers
    }

    #[test]
    fn signature_verification_is_exact_and_replay_bounded_in_both_directions() {
        let secret = "unit-test-signing-secret";
        let config = test_config(secret);
        let body = b"command=%2Fores-chatgpt&team_id=T1&channel_id=C1";
        let timestamp = 1_800_000_000_i64;
        let headers = signed_headers(secret, timestamp, body);

        assert!(verify_signature(&config, &headers, body, timestamp));
        assert!(verify_signature(&config, &headers, body, timestamp + 300));
        assert!(verify_signature(&config, &headers, body, timestamp - 300));
        assert!(!verify_signature(&config, &headers, body, timestamp + 301));
        assert!(!verify_signature(&config, &headers, body, timestamp - 301));
        assert!(!verify_signature(&config, &headers, b"tampered", timestamp));

        let mut forged = headers.clone();
        forged.insert(
            "x-slack-signature",
            HeaderValue::from_static(
                "v0=0000000000000000000000000000000000000000000000000000000000000000",
            ),
        );
        assert!(!verify_signature(&config, &forged, body, timestamp));

        let mut missing_signature = headers.clone();
        missing_signature.remove("x-slack-signature");
        assert!(!verify_signature(
            &config,
            &missing_signature,
            body,
            timestamp
        ));

        let mut missing_timestamp = headers.clone();
        missing_timestamp.remove("x-slack-request-timestamp");
        assert!(!verify_signature(
            &config,
            &missing_timestamp,
            body,
            timestamp
        ));

        let mut malformed_timestamp = headers.clone();
        malformed_timestamp.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_static("not-a-timestamp"),
        );
        assert!(!verify_signature(
            &config,
            &malformed_timestamp,
            body,
            timestamp
        ));

        let mut wrong_version = headers;
        wrong_version.insert(
            "x-slack-signature",
            HeaderValue::from_static(
                "v1=0000000000000000000000000000000000000000000000000000000000000000",
            ),
        );
        assert!(!verify_signature(&config, &wrong_version, body, timestamp));
    }

    #[test]
    fn signature_decoder_rejects_noncanonical_length_and_invalid_hex() {
        assert_eq!(decode_signature(&"00".repeat(32)), Some([0; 32]));
        assert!(decode_signature("00").is_none());
        assert!(decode_signature(&"gg".repeat(32)).is_none());
        assert!(decode_signature(&format!("{} ", "00".repeat(32))).is_none());
    }

    #[test]
    fn service_urls_allow_only_https_or_exact_loopback_plaintext() {
        assert_eq!(
            service_url("http://127.0.0.1:8142").expect("IPv4 loopback HTTP is allowed"),
            "http://127.0.0.1:8142/"
        );
        assert_eq!(
            service_url("http://127.0.0.2:8142").expect("127/8 loopback HTTP is allowed"),
            "http://127.0.0.2:8142/"
        );
        assert_eq!(
            service_url("http://localhost:8142").expect("localhost HTTP is allowed"),
            "http://localhost:8142/"
        );
        assert_eq!(
            service_url("http://[::1]:8160").expect("IPv6 loopback HTTP is allowed"),
            "http://[::1]:8160/"
        );
        assert_eq!(
            service_url("https://example.com/bridge").expect("remote HTTPS is allowed"),
            "https://example.com/bridge/"
        );

        assert!(service_url("http://example.com/bridge").is_err());
        assert!(service_url("http://localhost.attacker.example/bridge").is_err());
        assert!(service_url("http://127.0.0.1.attacker.example/bridge").is_err());
        assert!(service_url("https://user:secret@example.com/bridge").is_err());
        assert!(service_url("https://example.com/bridge?token=secret").is_err());
        assert!(service_url("https://example.com/bridge#fragment").is_err());
    }

    #[test]
    fn form_parser_rejects_duplicate_malformed_and_non_utf8_normalized_input() {
        assert!(parse_form(b"team_id=T1&team_id=T2").is_err());
        assert!(parse_form(b"team%5Fid=T1&team_id=T2").is_err());
        assert!(parse_form(b"team_id=%").is_err());
        assert!(parse_form(b"team_id=%GG").is_err());
        assert!(parse_form(b"team_id=%FF").is_err());
        assert_eq!(
            parse_form(b"text=hello+world%21")
                .expect("valid form")
                .get("text")
                .map(String::as_str),
            Some("hello world!")
        );
    }

    #[test]
    fn direct_command_alias_preserves_deterministic_routing_metadata() {
        let body = b"command=%2Fx-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Fix+DEN-1041+with+tests&trigger_id=trigger-1";
        let command = SlashCommand::parse(body).expect("valid Slack command alias");
        assert_eq!(command.provider(), Provider::Chatgpt);
        assert_eq!(command.text, "Fix DEN-1041 with tests");

        let request = RunRequest::direct(&command, 5).expect("valid run request");
        assert_eq!(request.provider, Provider::Chatgpt);
        assert_eq!(request.linear_issue.as_deref(), Some("DEN-1041"));
        assert_eq!(request.context_messages, 5);
        assert_eq!(request.run_id, run_id(&request.source_key));

        let duplicate = RunRequest::direct(&command, 5).expect("same run request");
        assert_eq!(request.run_id, duplicate.run_id);

        let other = SlashCommand::parse(
            b"command=%2Fx-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Fix+DEN-1041&trigger_id=trigger-2",
        )
        .expect("second Slack command");
        assert_ne!(
            request.run_id,
            RunRequest::direct(&other, 5)
                .expect("second run request")
                .run_id
        );
    }

    #[test]
    fn command_identifier_and_prompt_validation_enforce_bounded_inputs() {
        assert!(SlashCommand::parse(
            b"command=%2Funknown&team_id=T1&channel_id=C1&user_id=U1&trigger_id=t1"
        )
        .is_err());
        assert!(SlashCommand::parse(
            b"command=%2Fores-chatgpt&team_id=T1%2FT2&channel_id=C1&user_id=U1&trigger_id=t1"
        )
        .is_err());
        assert!(prompt("").is_err());
        assert!(prompt("contains\0nul").is_err());
        assert!(prompt(&"x".repeat(MAX_PROMPT_BYTES + 1)).is_err());
        assert_eq!(
            prompt("  bounded task  ").expect("valid prompt"),
            "bounded task"
        );
    }

    #[test]
    fn issue_detection_accepts_only_canonical_uppercase_identifiers() {
        assert_eq!(
            find_issue("please fix DEN-1041 now").as_deref(),
            Some("DEN-1041")
        );
        assert_eq!(find_issue("please fix den-1041 now"), None);
        assert_eq!(find_issue("please fix D-1 now"), None);
        assert_eq!(find_issue("please fix DEN-abc now"), None);
    }
}
