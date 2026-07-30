use ai_agent_bridge::providers::{
    ProviderClient, ProviderConfig, ProviderError, ProviderProtocol,
};

const ENV_NAME: &str = "DEN_318_OPENAI_API_KEY_MUST_NOT_EXIST";

fn config() -> ProviderConfig {
    ProviderConfig {
        name: "openai-test".into(),
        protocol: ProviderProtocol::OpenAiResponses,
        base_url: "https://api.openai.com/v1/".into(),
        model: "TEST_MODEL".into(),
        api_key_env: ENV_NAME.into(),
        allowed_hosts: vec!["api.openai.com".into()],
        timeout_secs: 30,
        connect_timeout_secs: 5,
        max_response_bytes: 1024,
    }
}

#[test]
fn serialized_provider_config_contains_only_the_environment_variable_name() {
    let serialized = serde_json::to_value(config()).expect("provider config should serialize");

    assert_eq!(serialized["api_key_env"], ENV_NAME);
    assert!(serialized.get("api_key").is_none());
    assert!(serialized.get("credential").is_none());
    assert!(serialized.get("token").is_none());
}

#[test]
fn missing_credential_error_exposes_only_the_environment_variable_name() {
    let error = match ProviderClient::from_config(config()) {
        Ok(_) => panic!("the reserved test environment variable must remain unset in CI"),
        Err(error) => error,
    };
    let rendered = error.to_string();

    assert!(rendered.contains(ENV_NAME));
    assert!(!rendered.contains("Bearer"));
    assert!(!rendered.contains("api.openai.com/v1/responses"));
}

#[test]
fn credential_error_format_does_not_claim_to_print_a_value_or_prefix() {
    let rendered = ProviderError::MissingCredential(ENV_NAME.into()).to_string();

    assert_eq!(
        rendered,
        format!("provider credential environment variable '{ENV_NAME}' is missing or empty")
    );
    assert!(!rendered.contains("value="));
    assert!(!rendered.contains("prefix="));
    assert!(!rendered.contains("hash="));
}

#[test]
fn invalid_serialized_config_cannot_smuggle_a_direct_api_key_field() {
    let raw = format!(
        r#"[{{"name":"openai","protocol":"open_ai_responses","base_url":"https://api.openai.com/v1/","model":"TEST_MODEL","api_key_env":"{ENV_NAME}","api_key":"must-not-be-accepted"}}]"#
    );

    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("fixture should be valid JSON");
    assert_eq!(parsed[0]["api_key"], "must-not-be-accepted");

    let configs = ai_agent_bridge::providers::parse_provider_configs(&raw)
        .expect("serde ignores unknown fields, so runtime credentials still come only from api_key_env");
    let reserialized = serde_json::to_value(&configs).expect("provider configs should serialize");
    assert!(reserialized[0].get("api_key").is_none());
    assert_eq!(reserialized[0]["api_key_env"], ENV_NAME);
}
