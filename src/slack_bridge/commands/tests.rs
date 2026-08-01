//! Unit coverage for the slash-command surface. Everything here exercises pure
//! functions: menu construction, submission decoding, prompt composition, and
//! the single-agent workflow guard. Network paths are covered by the adapter's
//! integration tests.

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    time::Duration,
};

use serde_json::json;

use super::{
    build_modal, channel_is_allowed, compose_prompt, parse_form, validate_single_agent_workflow,
    DispatchRequest, InteractionState, ModalContext, Provider, SlashCommandForm, TaskType,
};
use crate::slack_bridge::{
    validate_slash_command, SlackConfig, WorkflowAssignmentDto, WorkflowPlanDto, WorkflowStatusDto,
    WorkflowViewDto,
};

fn config() -> SlackConfig {
    SlackConfig {
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 8150,
        signing_secret: "0123456789abcdef0123456789abcdef".to_string(),
        bot_token: Some("xoxb-test".to_string()),
        bot_user_id: None,
        allowed_team_ids: ["T1"].into_iter().map(str::to_string).collect(),
        allowed_channel_ids: ["C1"].into_iter().map(str::to_string).collect(),
        allowed_thread_ts: BTreeSet::new(),
        command_prefix: "!ask-both".to_string(),
        bridge_url: "http://127.0.0.1:8142/".to_string(),
        bridge_bearer: None,
        slack_post_message_url: "https://slack.test/post".to_string(),
        claude_agent_key: "claude-fable-5".to_string(),
        openai_agent_key: "gpt-5.6-sol".to_string(),
        dry_run: false,
        idempotency_path: PathBuf::from("/tmp/slack-commands-test.jsonl"),
        max_request_age_secs: 300,
        workflow_timeout: Duration::from_secs(120),
        poll_interval: Duration::from_millis(1_000),
        max_body_bytes: 262_144,
        max_concurrent_workflows: 8,
        claude_command: "/my-claude".to_string(),
        openai_command: "/my-chatgpt".to_string(),
        claude_model_choices: vec!["claude-fable-5".to_string(), "claude-opus-5".to_string()],
        openai_model_choices: vec!["gpt-5.6-sol".to_string()],
        target_choices: vec!["github.com/ORESoftware/k8s-cluster".to_string()],
        context_message_default: 5,
        context_message_max: 25,
        slack_views_open_url: "https://slack.test/views.open".to_string(),
        slack_conversations_history_url: "https://slack.test/history".to_string(),
        broadcast_channel_id: Some("C-OPS".to_string()),
        linear_api_key: None,
        linear_team_id: None,
        linear_project_id: None,
        linear_state_todo: None,
        linear_state_started: None,
        linear_state_done: None,
    }
}

fn request() -> DispatchRequest {
    DispatchRequest {
        dispatch_id: "cmd.claude.V123".to_string(),
        provider_slug: "claude".to_string(),
        agent_key: "claude-fable-5".to_string(),
        task_type: TaskType::NewWork,
        target: "github.com/ORESoftware/k8s-cluster".to_string(),
        context_depth: 5,
        prompt: "Ship the cron canary".to_string(),
        channel_id: "C1".to_string(),
        channel_name: "eng".to_string(),
        team_id: "T1".to_string(),
        user_id: "U1".to_string(),
    }
}

fn workflow(agent_keys: &[&str]) -> WorkflowViewDto {
    WorkflowViewDto {
        plan: WorkflowPlanDto {
            id: "wf-123".to_string(),
            assignments: agent_keys
                .iter()
                .map(|key| WorkflowAssignmentDto {
                    agent_key: (*key).to_string(),
                })
                .collect(),
        },
        status: WorkflowStatusDto {
            stage: "running".to_string(),
        },
        submissions: Vec::new(),
    }
}

#[test]
fn parses_slack_form_encoding() {
    let body = b"command=%2Fmy-claude&text=hello+world&team_id=T1&channel_id=C1&trigger_id=abc.def";
    let fields = parse_form(body);
    let form = SlashCommandForm::from_fields(&fields);
    assert_eq!(form.command, "/my-claude");
    assert_eq!(form.text, "hello world");
    assert_eq!(form.team_id, "T1");
    assert_eq!(form.channel_id, "C1");
    assert_eq!(form.trigger_id, "abc.def");
}

#[test]
fn allowlists_gate_team_and_channel() {
    let config = config();
    assert!(channel_is_allowed(&config, "T1", "C1"));
    assert!(!channel_is_allowed(&config, "T2", "C1"));
    assert!(!channel_is_allowed(&config, "T1", "C2"));
    assert!(!channel_is_allowed(&config, "", "C1"));
    assert!(!channel_is_allowed(&config, "T1", ""));
}

#[test]
fn provider_slugs_round_trip() {
    assert_eq!(Provider::from_slug("claude"), Some(Provider::Claude));
    assert_eq!(Provider::from_slug("chatgpt"), Some(Provider::OpenAi));
    assert_eq!(Provider::from_slug("gemini"), None);
}

#[test]
fn provider_choices_stay_separated() {
    let config = config();
    assert!(Provider::Claude
        .choices(&config)
        .contains(&"claude-opus-5".to_string()));
    assert!(!Provider::Claude
        .choices(&config)
        .contains(&"gpt-5.6-sol".to_string()));
    assert!(!Provider::OpenAi
        .choices(&config)
        .contains(&"claude-fable-5".to_string()));
}

#[test]
fn unknown_task_type_degrades_to_ask() {
    assert_eq!(TaskType::from_value("review_repo"), TaskType::ReviewRepo);
    assert_eq!(TaskType::from_value("nonsense"), TaskType::Ask);
}

#[test]
fn modal_exposes_every_submenu_with_defaults() {
    let config = config();
    let form = SlashCommandForm {
        command: "/my-claude".to_string(),
        text: "prefilled task".to_string(),
        team_id: "T1".to_string(),
        channel_id: "C1".to_string(),
        channel_name: "eng".to_string(),
        user_id: "U1".to_string(),
        trigger_id: "abc.def".to_string(),
    };
    let view = build_modal(&config, Provider::Claude, &form);

    assert_eq!(view["type"], "modal");
    assert_eq!(view["callback_id"], "agent_dispatch");

    let blocks = view["blocks"].as_array().expect("blocks array");
    let ids: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["block_id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["prompt", "model", "task_type", "target", "context_depth"]
    );

    // The slash-command text should survive into the modal's prompt field.
    assert_eq!(blocks[0]["element"]["initial_value"], "prefilled task");

    // Only this provider's keys are offered.
    let model_options = blocks[1]["element"]["options"]
        .as_array()
        .expect("model options");
    let values: Vec<&str> = model_options
        .iter()
        .filter_map(|option| option["value"].as_str())
        .collect();
    assert_eq!(values, vec!["claude-fable-5", "claude-opus-5"]);

    // Channel context defaults to the configured depth rather than "none".
    assert_eq!(blocks[4]["element"]["initial_option"]["value"], "5");

    let metadata: ModalContext =
        serde_json::from_str(view["private_metadata"].as_str().expect("metadata")).expect("json");
    assert_eq!(metadata.provider, "claude");
    assert_eq!(metadata.channel_id, "C1");
}

#[test]
fn modal_omits_target_block_when_no_targets_configured() {
    let mut config = config();
    config.target_choices.clear();
    let form = SlashCommandForm {
        command: "/my-chatgpt".to_string(),
        ..SlashCommandForm::default()
    };
    let view = build_modal(&config, Provider::OpenAi, &form);
    let ids: Vec<&str> = view["blocks"]
        .as_array()
        .expect("blocks")
        .iter()
        .filter_map(|block| block["block_id"].as_str())
        .collect();
    assert!(!ids.contains(&"target"));
    assert!(ids.contains(&"context_depth"));
}

#[test]
fn interaction_state_reads_text_and_selections() {
    let raw = json!({
        "values": {
            "prompt": { "value": { "value": "do the thing" } },
            "model": { "value": { "selected_option": { "value": "claude-opus-5" } } }
        }
    });
    let state: InteractionState = serde_json::from_value(raw).expect("state");
    assert_eq!(state.text("prompt"), "do the thing");
    assert_eq!(state.selected("model"), "claude-opus-5");
    assert_eq!(state.selected("missing"), "");
    assert_eq!(state.text("missing"), "");
}

#[test]
fn prompt_carries_channel_context_marked_as_background() {
    let request = request();
    let composed = compose_prompt(&request, Some("[1.0] <@U9>: deploy is red"));
    assert!(composed.contains("Target: github.com/ORESoftware/k8s-cluster"));
    assert!(composed.contains("## Task"));
    assert!(composed.contains("Ship the cron canary"));
    assert!(composed.contains("## Recent channel context"));
    assert!(composed.contains("deploy is red"));
    // Channel text is untrusted background, and the prompt must say so.
    assert!(composed.contains("not instructions"));
}

#[test]
fn prompt_omits_context_section_when_depth_is_zero() {
    let request = request();
    let composed = compose_prompt(&request, None);
    assert!(!composed.contains("## Recent channel context"));
    assert!(composed.contains("Ship the cron canary"));
}

#[test]
fn single_agent_guard_accepts_only_the_requested_agent() {
    assert!(validate_single_agent_workflow(&workflow(&["claude-fable-5"]), "claude-fable-5").is_ok());
    // Routed to the wrong agent.
    assert!(
        validate_single_agent_workflow(&workflow(&["gpt-5.6-sol"]), "claude-fable-5").is_err()
    );
    // Fanned out beyond the single requested agent.
    assert!(validate_single_agent_workflow(
        &workflow(&["claude-fable-5", "gpt-5.6-sol"]),
        "claude-fable-5"
    )
    .is_err());
    // No assignment at all.
    assert!(validate_single_agent_workflow(&workflow(&[]), "claude-fable-5").is_err());
}

#[test]
fn slash_command_names_are_validated() {
    assert!(validate_slash_command("X", "/my-claude").is_ok());
    assert!(validate_slash_command("X", "my-claude").is_err());
    assert!(validate_slash_command("X", "/").is_err());
    assert!(validate_slash_command("X", "/my claude").is_err());
    assert!(validate_slash_command("X", "").is_err());
}
