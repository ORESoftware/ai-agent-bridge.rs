/// Build the reviewed run modal for fixture generation and preview tooling.
///
/// `modal` is private and reached only through `views.open`, which makes the
/// rendered submenu hard to assert on outside a full ingress round trip. This
/// wrapper exposes the same builder — no second copy of the layout — so
/// `tests/slack_modal_fixture.rs` can freeze the payload and the Chromium spec
/// in `tests/browser/specs/modal.spec.mjs` can assert what an operator actually
/// sees.
///
/// `private_metadata` is supplied by the caller because the real value carries
/// per-run routing state; previews pass a fixed placeholder so the fixture stays
/// deterministic.
pub fn preview_run_modal(
    provider_command: &str,
    binding: &ChannelProjectBinding,
    private_metadata: &str,
    context_messages: usize,
) -> Option<Value> {
    let provider = Provider::from_command(provider_command)?;
    Some(modal(provider, binding, private_metadata, context_messages))
}

#[cfg(test)]
mod preview_run_modal_tests {
    use super::*;
    use crate::slack_project_bindings::BudgetPolicy;

    fn binding(write_policy: WritePolicy) -> ChannelProjectBinding {
        ChannelProjectBinding {
            workspace_id: "T01B3C83PMK".into(),
            channel_id: "C0BKP2N3LG7".into(),
            linear_team_id: "team-uuid".into(),
            linear_team_key: "DEN".into(),
            linear_project_id: "project-uuid".into(),
            default_repository: "oresoftware/k8s-cluster".into(),
            repository_allowlist: ["oresoftware/k8s-cluster".to_string()]
                .into_iter()
                .collect(),
            default_agent_mode: AgentMode::Claude,
            allowed_agent_modes: [AgentMode::Claude].into_iter().collect(),
            allowed_user_ids: ["U1".to_string()].into_iter().collect(),
            allowed_user_group_ids: Default::default(),
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

    #[test]
    fn every_reviewed_command_alias_resolves_to_a_modal() {
        for command in [
            "/ores-claude",
            "/x-claude",
            "/my-claude",
            "/ores-chatgpt",
            "/x-chatgpt",
            "/my-chatgpt",
        ] {
            assert!(
                preview_run_modal(command, &binding(WritePolicy::DraftPullRequest), "m", 5)
                    .is_some(),
                "{command} must open the reviewed modal"
            );
        }
        assert!(
            preview_run_modal("/x-gemini", &binding(WritePolicy::DraftPullRequest), "m", 5)
                .is_none(),
            "an unreviewed command must not open the modal"
        );
    }

    #[test]
    fn the_preview_is_the_same_payload_the_ingress_opens() {
        let binding = binding(WritePolicy::DraftPullRequest);
        assert_eq!(
            preview_run_modal("/x-claude", &binding, "meta", 5).unwrap(),
            modal(Provider::Claude, &binding, "meta", 5),
            "the preview must not drift from the builder used by views.open"
        );
    }

    #[test]
    fn write_scope_options_never_exceed_the_channel_policy() {
        let read_only = preview_run_modal("/x-claude", &binding(WritePolicy::ReadOnly), "m", 5)
            .unwrap()
            .to_string();
        assert!(!read_only.contains("draft_pull_request"));
        assert!(!read_only.contains("linear_write"));

        let linear_only = preview_run_modal("/x-claude", &binding(WritePolicy::LinearOnly), "m", 5)
            .unwrap()
            .to_string();
        assert!(linear_only.contains("linear_write"));
        assert!(!linear_only.contains("draft_pull_request"));
    }
}

#[cfg(test)]
mod block_kit_limit_tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{modal, Provider};
    use crate::slack_project_bindings::{
        AgentMode, BudgetPolicy, ChannelProjectBinding, WritePolicy,
    };

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
