#[cfg(test)]
mod block_kit_limit_tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{modal, Provider};
    use crate::slack_project_bindings::{
        AgentMode, BudgetPolicy, ChannelProjectBinding, WritePolicy,
    };

    /// Slack rejects a `views.open` payload that breaches any documented Block
    /// Kit ceiling, and the member only ever sees "the dialog could not be
    /// opened". A longer provider label, an extra menu entry or a wider
    /// repository allowlist are each enough to trip one.
    ///
    /// These are deliberately API-shape limits rather than rendering checks: a
    /// payload Slack refuses outright still renders perfectly well in a browser,
    /// so the Chromium coverage over the frozen fixtures cannot catch them.
    fn binding(repositories: usize, write_policy: WritePolicy) -> ChannelProjectBinding {
        let repository_allowlist: BTreeSet<String> = (0..repositories)
            .map(|index| format!("oresoftware/repo-{index:03}"))
            .collect();
        let default_repository = repository_allowlist
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "oresoftware/repo-000".to_string());

        ChannelProjectBinding {
            workspace_id: "T01B3C83PMK".into(),
            channel_id: "C0BKP2N3LG7".into(),
            linear_team_id: "team-uuid".into(),
            linear_team_key: "DEN".into(),
            linear_project_id: "project-uuid".into(),
            default_repository,
            repository_allowlist,
            default_agent_mode: AgentMode::Claude,
            allowed_agent_modes: [AgentMode::Claude, AgentMode::Chatgpt].into_iter().collect(),
            allowed_user_ids: ["U1".to_string()].into_iter().collect(),
            allowed_user_group_ids: BTreeSet::new(),
            write_policy,
            budget_policy: BudgetPolicy {
                max_concurrent_runs: 2,
                max_runtime_secs: 600,
                max_tokens: 100_000,
                max_spend_cents: 500,
                max_retries: 2,
            },
            updated_by: "U1".into(),
            updated_at: "2026-08-01T12:00:00Z".into(),
        }
    }

    fn assert_within_block_kit_limits(view: &Value) {
        // Modal titles are capped at 24 characters; Slack hard-rejects longer.
        let title = view["title"]["text"].as_str().expect("title");
        assert!(
            title.chars().count() <= 24,
            "modal title {title:?} exceeds 24 characters",
        );
        for key in ["submit", "close"] {
            let label = view[key]["text"].as_str().expect(key);
            assert!(
                label.chars().count() <= 24,
                "{key} label {label:?} exceeds 24 characters",
            );
        }

        let metadata = view["private_metadata"].as_str().expect("private_metadata");
        assert!(
            metadata.len() <= 3_000,
            "private_metadata exceeds 3000 bytes",
        );

        let blocks = view["blocks"].as_array().expect("blocks");
        assert!(!blocks.is_empty(), "a modal must carry at least one block");
        assert!(blocks.len() <= 100, "a view accepts at most 100 blocks");

        for block in blocks {
            let block_id = block["block_id"].as_str().expect("block_id");
            assert!(block_id.len() <= 255, "block_id too long: {block_id}");
            assert!(
                block["label"]["text"]
                    .as_str()
                    .is_some_and(|label| label.chars().count() <= 2_000),
                "{block_id} label exceeds 2000 characters",
            );

            let element = &block["element"];
            let action_id = element["action_id"].as_str().expect("action_id");
            assert!(action_id.len() <= 255, "action_id too long: {action_id}");

            if let Some(max_length) = element["max_length"].as_u64() {
                assert!(
                    max_length <= 3_000,
                    "{block_id} plain_text_input max_length exceeds 3000",
                );
            }

            if let Some(options) = element["options"].as_array() {
                assert!(!options.is_empty(), "{block_id} has an empty menu");
                assert!(options.len() <= 100, "{block_id} exceeds 100 options");
                for option in options {
                    let text = option["text"]["text"].as_str().expect("option text");
                    let value = option["value"].as_str().expect("option value");
                    assert!(!text.is_empty(), "{block_id} has a blank option label");
                    assert!(
                        text.chars().count() <= 75,
                        "{block_id} option label too long: {text}",
                    );
                    assert!(value.len() <= 150, "{block_id} option value too long");
                }
                // Slack rejects a view whose initial_option is absent from its
                // own options list. `Config::from_env` currently constrains the
                // context default to exactly the offered values, and the
                // repository menu is bounded by MAX_REPOSITORIES_PER_BINDING —
                // this keeps both true if either constraint is ever relaxed.
                if let Some(initial) = element.get("initial_option") {
                    assert!(
                        options.contains(initial),
                        "{block_id} preselects an option absent from its menu: {initial}",
                    );
                }
            }
        }
    }

    #[test]
    fn every_modal_shape_respects_slack_block_kit_limits() {
        // The widest realistic shape: a full repository allowlist, every write
        // policy, every offered context depth, and the largest private_metadata
        // a run correlation produces.
        let metadata = "m".repeat(2_000);
        for provider in [Provider::Claude, Provider::Chatgpt] {
            for write_policy in [
                WritePolicy::ReadOnly,
                WritePolicy::LinearOnly,
                WritePolicy::DraftPullRequest,
            ] {
                for context_messages in [0, 5, 10, 20] {
                    let view = modal(
                        provider,
                        &binding(100, write_policy),
                        &metadata,
                        context_messages,
                    );
                    assert_within_block_kit_limits(&view);
                }
            }
        }
    }

    #[test]
    fn the_repository_menu_never_drops_the_default_repository() {
        // The menu is truncated to Slack's 100-option ceiling. A default outside
        // the retained slice would preselect an option the member cannot see,
        // which Slack refuses to render.
        let binding = binding(100, WritePolicy::DraftPullRequest);
        let view = modal(Provider::Claude, &binding, "m", 5);

        let repository_block = view["blocks"]
            .as_array()
            .expect("blocks")
            .iter()
            .find(|block| block["block_id"] == "repository")
            .expect("repository block");
        let options = repository_block["element"]["options"]
            .as_array()
            .expect("options");

        assert!(
            options
                .iter()
                .any(|option| option["value"] == binding.default_repository.as_str()),
            "the default repository must remain selectable",
        );
    }
}
