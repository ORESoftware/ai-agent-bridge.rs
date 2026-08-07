#[cfg(test)]
mod block_kit_contract_tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use serde_json::Value;

    use super::{modal, Provider};
    use crate::slack_project_bindings::{
        AgentMode, BudgetPolicy, ChannelProjectBinding, WritePolicy,
    };

    /// Slack rejects a `views.open` payload that breaches any documented Block
    /// Kit ceiling, and the member only ever sees "the dialog could not be
    /// opened". A longer provider label, an extra menu entry, or a wider
    /// repository allowlist are all easy ways to trip one, so the ceilings are
    /// asserted here rather than discovered in a channel.
    fn binding(repositories: usize, write_policy: WritePolicy) -> ChannelProjectBinding {
        let allowlist: BTreeSet<String> = (0..repositories)
            .map(|index| format!("oresoftware/repo-{index:03}"))
            .collect();
        let default_repository = allowlist
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "oresoftware/repo-000".to_string());

        ChannelProjectBinding {
            workspace_id: "T01B3C83PMK".to_string(),
            channel_id: "C1".to_string(),
            linear_team_id: "team-eb8ab169".to_string(),
            linear_team_key: "DEN".to_string(),
            linear_project_id: "project-1".to_string(),
            default_repository,
            repository_allowlist: allowlist,
            default_agent_mode: AgentMode::Claude,
            allowed_agent_modes: [AgentMode::Claude, AgentMode::Chatgpt].into_iter().collect(),
            allowed_user_ids: ["U01OPERATOR".to_string()].into_iter().collect(),
            allowed_user_group_ids: BTreeSet::new(),
            write_policy,
            budget_policy: BudgetPolicy {
                max_concurrent_runs: 2,
                max_runtime_secs: 900,
                max_tokens: 500_000,
                max_spend_cents: 500,
                max_retries: 2,
            },
            updated_by: "U01OPERATOR".to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
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
            assert!(label.chars().count() <= 24, "{key} label exceeds 24 chars");
        }

        let metadata = view["private_metadata"].as_str().expect("private_metadata");
        assert!(metadata.len() <= 3_000, "private_metadata exceeds 3000 bytes");

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
    fn both_provider_modals_respect_slack_block_kit_limits() {
        // The widest realistic shape: a full repository allowlist, the broadest
        // write policy, and the largest private_metadata a run correlation
        // produces.
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

    /// Writes the exact payload the adapter hands to `views.open` so the Block
    /// Kit browser contract renders the real thing rather than a hand-kept copy
    /// that drifts from the code.
    #[test]
    fn emits_block_kit_fixtures_for_the_browser_contract() {
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/blockkit");
        fs::create_dir_all(&out).expect("fixture directory");

        for (provider, name) in [
            (Provider::Claude, "ores-claude"),
            (Provider::Chatgpt, "ores-chatgpt"),
        ] {
            let view = modal(
                provider,
                &binding(3, WritePolicy::DraftPullRequest),
                "browser-contract",
                5,
            );
            let rendered = serde_json::to_string_pretty(&view).expect("serialize view");
            fs::write(out.join(format!("{name}.json")), rendered).expect("write fixture");
        }

        assert!(out.join("ores-claude.json").exists());
        assert!(out.join("ores-chatgpt.json").exists());
    }
}
