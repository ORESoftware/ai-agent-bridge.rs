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
    build_modal, channel_is_allowed, compose_prompt, parse_form, render_context,
    validate_single_agent_workflow, DispatchRequest, HistoryMessage, InteractionState,
    ModalContext, Provider, SlashCommandForm, TaskType,
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
        linear_include_channel_context: false,
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
    assert!(
        validate_single_agent_workflow(&workflow(&["claude-fable-5"]), "claude-fable-5").is_ok()
    );
    // Routed to the wrong agent.
    assert!(validate_single_agent_workflow(&workflow(&["gpt-5.6-sol"]), "claude-fable-5").is_err());
    // Fanned out beyond the single requested agent.
    assert!(validate_single_agent_workflow(
        &workflow(&["claude-fable-5", "gpt-5.6-sol"]),
        "claude-fable-5"
    )
    .is_err());
    // No assignment at all.
    assert!(validate_single_agent_workflow(&workflow(&[]), "claude-fable-5").is_err());
}

// --- channel context selection -------------------------------------------

fn human(ts: &str, user: &str, text: &str) -> HistoryMessage {
    HistoryMessage {
        text: text.to_string(),
        user: user.to_string(),
        bot_id: String::new(),
        ts: ts.to_string(),
        subtype: String::new(),
    }
}

fn bot(ts: &str, text: &str) -> HistoryMessage {
    HistoryMessage {
        text: text.to_string(),
        user: String::new(),
        bot_id: "B1".to_string(),
        ts: ts.to_string(),
        subtype: String::new(),
    }
}

#[test]
fn context_excludes_bot_output_to_prevent_feedback() {
    // Slack returns newest-first. The adapter posts its own acknowledgements and
    // model replies into this same channel; re-ingesting them would feed the
    // model its own prior output on the next dispatch.
    let messages = vec![
        bot("5.0", "*Agent task dispatched* — claude-fable-5"),
        human("4.0", "U2", "the deploy is red"),
        bot("3.0", "alertmanager: CPU high"),
        human("2.0", "U1", "who is on call?"),
    ];
    let rendered = render_context(&messages, 5, None).expect("context");

    assert!(!rendered.contains("Agent task dispatched"));
    assert!(!rendered.contains("alertmanager"));
    assert!(rendered.contains("the deploy is red"));
    assert!(rendered.contains("who is on call?"));
}

#[test]
fn context_excludes_the_configured_bot_user() {
    // A bot posting as a user carries a `user` id rather than a `bot_id`.
    let messages = vec![
        human("2.0", "UBOT", "posted by this app"),
        human("1.0", "U1", "posted by a human"),
    ];
    let rendered = render_context(&messages, 5, Some("UBOT")).expect("context");

    assert!(!rendered.contains("posted by this app"));
    assert!(rendered.contains("posted by a human"));
}

#[test]
fn context_is_rendered_oldest_first_with_authors() {
    let messages = vec![
        human("3.0", "U3", "third"),
        human("2.0", "U2", "second"),
        human("1.0", "U1", "first"),
    ];
    let rendered = render_context(&messages, 5, None).expect("context");

    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "[1.0] <@U1>: first");
    assert_eq!(lines[2], "[3.0] <@U3>: third");
}

#[test]
fn context_takes_the_newest_messages_up_to_depth() {
    let messages = vec![
        human("4.0", "U4", "newest"),
        human("3.0", "U3", "newer"),
        human("2.0", "U2", "older"),
        human("1.0", "U1", "oldest"),
    ];
    let rendered = render_context(&messages, 2, None).expect("context");

    assert!(rendered.contains("newest"));
    assert!(rendered.contains("newer"));
    assert!(!rendered.contains("older"));
    assert!(!rendered.contains("oldest"));
    // Still oldest-first within the selection.
    assert!(rendered.find("newer").unwrap() < rendered.find("newest").unwrap());
}

#[test]
fn context_depth_counts_humans_not_raw_messages() {
    // Filtering happens before the depth cut, so interleaved bot noise cannot
    // starve the transcript down to nothing.
    let messages = vec![
        bot("6.0", "noise"),
        human("5.0", "U5", "keep me"),
        bot("4.0", "noise"),
        human("3.0", "U3", "keep me too"),
        bot("2.0", "noise"),
        human("1.0", "U1", "and me"),
    ];
    let rendered = render_context(&messages, 3, None).expect("context");

    assert_eq!(rendered.lines().count(), 3);
    assert!(!rendered.contains("noise"));
}

#[test]
fn context_skips_tombstones_blank_text_and_authorless_messages() {
    let mut joined = human("3.0", "U3", "has joined the channel");
    joined.subtype = "channel_join".to_string();
    let blank = human("2.0", "U2", "   ");
    let authorless = HistoryMessage {
        text: "no author".to_string(),
        user: String::new(),
        bot_id: String::new(),
        ts: "1.5".to_string(),
        subtype: String::new(),
    };
    let messages = vec![joined, blank, authorless, human("1.0", "U1", "real")];

    let rendered = render_context(&messages, 5, None).expect("context");
    assert_eq!(rendered, "[1.0] <@U1>: real");
}

#[test]
fn context_is_none_when_nothing_survives_the_filter() {
    assert_eq!(render_context(&[bot("1.0", "only bots")], 5, None), None);
    assert_eq!(render_context(&[], 5, None), None);
    // Depth zero means the member asked for no context at all.
    assert_eq!(render_context(&[human("1.0", "U1", "hi")], 0, None), None);
}

#[test]
fn context_truncates_an_oversized_single_message() {
    let long = "x".repeat(5_000);
    let rendered = render_context(&[human("1.0", "U1", &long)], 1, None).expect("context");
    assert!(rendered.len() < 2_000);
    assert!(rendered.contains("truncated"));
}

/// Writes the exact modal payload the adapter would hand to `views.open` so the
/// Block Kit browser contract check renders the real thing rather than a
/// hand-maintained copy that can drift away from the code.
#[test]
fn emits_block_kit_fixtures_for_the_browser_contract() {
    use std::{fs, path::Path};

    let config = config();
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/blockkit");
    fs::create_dir_all(&out).expect("fixture directory");

    for (provider, name) in [
        (Provider::Claude, "my-claude"),
        (Provider::OpenAi, "my-chatgpt"),
    ] {
        let form = SlashCommandForm {
            command: format!("/{name}"),
            text: "harden the cron canary rollout".to_string(),
            team_id: "T1".to_string(),
            channel_id: "C1".to_string(),
            channel_name: "eng".to_string(),
            user_id: "U1".to_string(),
            trigger_id: "abc.def".to_string(),
        };
        let view = build_modal(&config, provider, &form);
        let rendered = serde_json::to_string_pretty(&view).expect("serialize view");
        fs::write(out.join(format!("{name}.json")), rendered).expect("write fixture");
    }

    assert!(out.join("my-claude.json").exists());
    assert!(out.join("my-chatgpt.json").exists());
}

#[test]
fn slash_command_names_are_validated() {
    assert!(validate_slash_command("X", "/my-claude").is_ok());
    assert!(validate_slash_command("X", "my-claude").is_err());
    assert!(validate_slash_command("X", "/").is_err());
    assert!(validate_slash_command("X", "/my claude").is_err());
    assert!(validate_slash_command("X", "").is_err());
}
