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
