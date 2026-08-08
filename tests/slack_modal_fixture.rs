//! Freeze the reviewed run modal so the Chromium spec asserts a real payload.
//!
//! `tests/browser/specs/modal.spec.mjs` renders these fixtures and checks what an
//! operator sees when `/x-claude` or `/x-chatgpt` is run bare. That is only
//! meaningful while the fixtures match what `views.open` actually sends, so this
//! test regenerates them and fails on drift.
//!
//! Refresh after an intentional modal change:
//!
//! ```text
//! UPDATE_SLACK_MODAL_FIXTURES=1 cargo test --test slack_modal_fixture
//! ```

use std::{collections::BTreeSet, fs, path::PathBuf};

use ai_agent_bridge::{
    slack_commands::preview_run_modal,
    slack_project_bindings::{AgentMode, BudgetPolicy, ChannelProjectBinding, WritePolicy},
};

/// Fixed so the fixtures stay byte-stable; the live value carries run routing.
const PREVIEW_METADATA: &str = "preview-metadata-0000";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("browser")
        .join("fixtures")
}

fn binding(write_policy: WritePolicy, repositories: &[&str]) -> ChannelProjectBinding {
    ChannelProjectBinding {
        workspace_id: "T01B3C83PMK".into(),
        channel_id: "C0BKP2N3LG7".into(),
        linear_team_id: "eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc".into(),
        linear_team_key: "DEN".into(),
        linear_project_id: "72e891e2-603d-4903-8d08-bd06d204520f".into(),
        default_repository: repositories[0].into(),
        repository_allowlist: repositories
            .iter()
            .map(|repository| (*repository).to_string())
            .collect::<BTreeSet<_>>(),
        default_agent_mode: AgentMode::Claude,
        allowed_agent_modes: [AgentMode::Claude, AgentMode::Chatgpt]
            .into_iter()
            .collect(),
        allowed_user_ids: ["U01AZNU2LJ2".to_string()].into_iter().collect(),
        allowed_user_group_ids: BTreeSet::new(),
        write_policy,
        budget_policy: BudgetPolicy {
            max_concurrent_runs: 2,
            max_runtime_secs: 900,
            max_tokens: 200_000,
            max_spend_cents: 500,
            max_retries: 1,
        },
        updated_by: "U01AZNU2LJ2".into(),
        updated_at: "2026-08-01T12:00:00Z".into(),
    }
}

/// The two fixtures deliberately differ in write policy and repository count so
/// the browser spec covers both the permissive and the restricted rendering.
fn cases() -> Vec<(&'static str, &'static str, ChannelProjectBinding, usize)> {
    vec![
        (
            "modal.claude.draft-pull-request.json",
            "/x-claude",
            binding(
                WritePolicy::DraftPullRequest,
                &["oresoftware/k8s-cluster", "oresoftware/ai-agent-bridge.rs"],
            ),
            5,
        ),
        (
            "modal.chatgpt.read-only.json",
            "/x-chatgpt",
            binding(WritePolicy::ReadOnly, &["oresoftware/k8s-cluster"]),
            5,
        ),
    ]
}

#[test]
fn the_browser_fixtures_match_the_modal_builder() {
    let update = std::env::var_os("UPDATE_SLACK_MODAL_FIXTURES").is_some();
    let directory = fixtures_dir();
    if update {
        fs::create_dir_all(&directory).expect("create fixtures directory");
    }

    for (name, command, binding, context_messages) in cases() {
        let view = preview_run_modal(command, &binding, PREVIEW_METADATA, context_messages)
            .unwrap_or_else(|| panic!("{command} must resolve to a reviewed provider"));
        let rendered = format!("{}\n", serde_json::to_string_pretty(&view).unwrap());
        let path = directory.join(name);

        if update {
            fs::write(&path, &rendered).expect("write fixture");
            continue;
        }

        let committed = fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing {}; regenerate with UPDATE_SLACK_MODAL_FIXTURES=1",
                path.display()
            )
        });
        assert_eq!(
            committed, rendered,
            "{name} is stale; regenerate with UPDATE_SLACK_MODAL_FIXTURES=1"
        );
    }
}

#[test]
fn the_default_context_depth_is_five_channel_messages() {
    let binding = binding(WritePolicy::DraftPullRequest, &["oresoftware/k8s-cluster"]);
    let view = preview_run_modal("/x-claude", &binding, PREVIEW_METADATA, 5).unwrap();
    let block = view["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["block_id"] == "context_messages")
        .expect("the modal must offer a channel-context selection");
    assert_eq!(block["element"]["initial_option"]["value"], "5");
    assert_eq!(
        block["element"]["initial_option"]["text"]["text"],
        "Last 5 messages (default)"
    );
}

#[test]
fn only_the_linear_issue_field_is_optional() {
    let binding = binding(WritePolicy::DraftPullRequest, &["oresoftware/k8s-cluster"]);
    let view = preview_run_modal("/x-claude", &binding, PREVIEW_METADATA, 5).unwrap();
    for block in view["blocks"].as_array().unwrap() {
        let optional = block["optional"].as_bool().unwrap_or(false);
        let expected = block["block_id"] == "issue";
        assert_eq!(
            optional, expected,
            "block {} has the wrong optionality",
            block["block_id"]
        );
    }
}

#[test]
fn the_repository_menu_never_offers_a_repository_outside_the_allowlist() {
    let binding = binding(
        WritePolicy::DraftPullRequest,
        &["oresoftware/k8s-cluster", "oresoftware/ai-agent-bridge.rs"],
    );
    let view = preview_run_modal("/x-claude", &binding, PREVIEW_METADATA, 5).unwrap();
    let block = view["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["block_id"] == "repository")
        .expect("the modal must offer a repository selection");
    let offered: BTreeSet<String> = block["element"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(offered, binding.repository_allowlist);
}
