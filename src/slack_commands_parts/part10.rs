#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_config(signing_secret: &str) -> Config {
        Config {
            host: "127.0.0.1".parse().expect("loopback IP"),
            port: DEFAULT_PORT,
            signing_secret: signing_secret.to_string(),
            bot_token: "xoxb-test-only".to_string(),
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
    fn signature_verification_is_exact_and_replay_bounded() {
        let secret = "unit-test-signing-secret";
        let config = test_config(secret);
        let body = b"command=%2Fores-chatgpt&team_id=T1&channel_id=C1";
        let now = 1_800_000_000_i64;
        let headers = signed_headers(secret, now, body);

        assert!(verify_signature(&config, &headers, body, now));
        assert!(verify_signature(&config, &headers, body, now + 300));
        assert!(!verify_signature(&config, &headers, body, now + 301));
        assert!(!verify_signature(&config, &headers, b"tampered", now));

        let mut forged = headers.clone();
        forged.insert(
            "x-slack-signature",
            HeaderValue::from_static(
                "v0=0000000000000000000000000000000000000000000000000000000000000000",
            ),
        );
        assert!(!verify_signature(&config, &forged, body, now));

        let mut missing = headers;
        missing.remove("x-slack-signature");
        assert!(!verify_signature(&config, &missing, body, now));
    }

    #[test]
    fn signature_decoder_rejects_noncanonical_input() {
        assert_eq!(decode_signature(&"00".repeat(32)), Some([0; 32]));
        assert!(decode_signature("00").is_none());
        assert!(decode_signature(&"gg".repeat(32)).is_none());
        assert!(decode_signature(&format!("{} ", "00".repeat(32))).is_none());
    }

    #[test]
    fn service_urls_fail_closed_for_remote_plaintext_and_embedded_credentials() {
        assert_eq!(
            service_url("http://127.0.0.1:8142").expect("loopback HTTP is allowed"),
            "http://127.0.0.1:8142/"
        );
        assert_eq!(
            service_url("http://[::1]:8160").expect("IPv6 loopback HTTP is allowed"),
            "http://[::1]:8160/"
        );
        assert!(service_url("http://example.com/bridge").is_err());
        assert!(service_url("https://user:secret@example.com/bridge").is_err());
        assert!(service_url("https://example.com/bridge?token=secret").is_err());
        assert!(service_url("https://example.com/bridge#fragment").is_err());
    }

    #[test]
    fn form_parser_rejects_duplicate_and_malformed_normalized_keys() {
        assert!(parse_form(b"team_id=T1&team_id=T2").is_err());
        assert!(parse_form(b"team%5Fid=T1&team_id=T2").is_err());
        assert!(parse_form(b"team_id=%").is_err());
        assert!(parse_form(b"team_id=%GG").is_err());
        assert_eq!(
            parse_form(b"text=hello+world%21")
                .expect("valid form")
                .get("text")
                .map(String::as_str),
            Some("hello world!")
        );
    }

    #[test]
    fn direct_command_parsing_preserves_deterministic_routing_metadata() {
        let body = b"command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Fix+DEN-1041+with+tests&trigger_id=trigger-1";
        let command = SlashCommand::parse(body).expect("valid Slack command");
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
            b"command=%2Fores-chatgpt&team_id=T1&channel_id=C1&user_id=U1&text=Fix+DEN-1041&trigger_id=trigger-2",
        )
        .expect("second Slack command");
        assert_ne!(
            request.run_id,
            RunRequest::direct(&other, 5).expect("second run request").run_id
        );
    }

    #[test]
    fn command_and_prompt_validation_enforce_bounded_inputs() {
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
        assert_eq!(prompt("  bounded task  ").expect("valid prompt"), "bounded task");
    }

    #[test]
    fn issue_detection_accepts_only_canonical_uppercase_identifiers() {
        assert_eq!(find_issue("please fix DEN-1041 now").as_deref(), Some("DEN-1041"));
        assert_eq!(find_issue("please fix den-1041 now"), None);
        assert_eq!(find_issue("please fix D-1 now"), None);
        assert_eq!(find_issue("please fix DEN-abc now"), None);
    }
}
