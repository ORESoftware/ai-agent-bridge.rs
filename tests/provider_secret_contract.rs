use std::sync::Mutex;

use ai_agent_bridge::providers::{ProviderClient, ProviderConfig, ProviderProtocol};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV_NAME: &str = "DEN_318_OPENAI_API_KEY_TEST";

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
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var(ENV_NAME);

    let error = match ProviderClient::from_config(config()) {
        Ok(_) => panic!("provider startup must fail when its credential is absent"),
        Err(error) => error,
    };
    let rendered = error.to_string();

    assert!(rendered.contains(ENV_NAME));
    assert!(!rendered.contains("Bearer"));
    assert!(!rendered.contains("api.openai.com/v1/responses"));
}

#[test]
fn invalid_configuration_does_not_echo_a_loaded_credential() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let credential = "provider-test-secret-value-not-for-production";
    std::env::set_var(ENV_NAME, credential);

    let mut invalid = config();
    invalid.base_url = "http://api.openai.com/v1/".into();
    let error = match ProviderClient::from_config(invalid) {
        Ok(_) => panic!("remote provider URLs must require HTTPS"),
        Err(error) => error,
    };
    std::env::remove_var(ENV_NAME);

    let rendered = error.to_string();
    assert!(!rendered.contains(credential));
    assert!(!rendered.contains("Authorization"));
}

#[test]
fn whitespace_only_credential_fails_closed_without_echoing_the_value() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var(ENV_NAME, " \n\t ");

    let error = match ProviderClient::from_config(config()) {
        Ok(_) => panic!("whitespace-only credentials must be rejected"),
        Err(error) => error,
    };
    std::env::remove_var(ENV_NAME);

    let rendered = error.to_string();
    assert!(rendered.contains(ENV_NAME));
    assert!(!rendered.contains('\n'));
    assert!(!rendered.contains('\t'));
}
