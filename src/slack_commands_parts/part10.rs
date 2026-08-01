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
            bot_token: "xoxb-test-token".into(),
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
