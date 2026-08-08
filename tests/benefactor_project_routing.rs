//! Fail-closed routing contract for the reviewed Benefactor repository family.
//!
//! The canonical Slack binding keeps the MCP server as its default target, but
//! issue-bound draft-PR work may explicitly select another reviewed Benefactor
//! repository. Generated output and the misspelled legacy site remain excluded.

use std::collections::BTreeSet;

use ai_agent_bridge::slack_project_bindings::{
    RegistryError, RequestedCapability, ResolveRequest, SlackProjectRegistry, WritePolicy,
};

const WORKSPACE: &str = "T01B3C83PMK";
const CHANNEL: &str = "C0BKP367N95";
const OPERATOR: &str = "U01AZNU2LJ2";
const DEFAULT_REPOSITORY: &str = "benefactor-cc/benefactor-cc-mcp-server.rs";

const REVIEWED_REPOSITORIES: [&str; 14] = [
    "benefactor-cc/benefactor-cc-mcp-server.rs",
    "benefactor-cc/backend.rs",
    "benefactor-cc/benefactor-sync",
    "benefactor-cc/benefactor-interfaces",
    "benefactor-cc/benefactor-e2e",
    "benefactor-cc/benefactor-automations",
    "benefactor-cc/benefactor-sendgrid-outreach",
    "benefactor-cc/benefactor-monorepo",
    "benefactor-cc/benefactor-lib",
    "benefactor-cc/benefactor-clients",
    "benefactor-cc/benefactor-cli",
    "benefactor-cc/.github",
    "ORESoftware/benefactor.cc",
    "ORESoftware/k8s-cluster",
];

fn registry() -> SlackProjectRegistry {
    SlackProjectRegistry::from_json(include_bytes!(
        "../config/alex-main-agent.channels.json"
    ))
    .expect("canonical alex-main-agent registry must load")
}

fn request(repository: Option<&str>) -> ResolveRequest {
    ResolveRequest {
        workspace_id: WORKSPACE.to_string(),
        channel_id: CHANNEL.to_string(),
        user_id: OPERATOR.to_string(),
        user_group_ids: BTreeSet::new(),
        requested_repository: repository.map(str::to_string),
        requested_agent_mode: None,
        requested_capability: RequestedCapability::RepositoryWrite,
        linear_issue_identifier: Some("DEN-3009".to_string()),
    }
}

#[test]
fn default_and_every_reviewed_repository_resolve_to_draft_pr_policy() {
    let registry = registry();

    let default = registry
        .resolve(&request(None))
        .expect("the default Benefactor repository must resolve");
    assert_eq!(default.repository, DEFAULT_REPOSITORY);
    assert_eq!(default.write_policy, WritePolicy::DraftPullRequest);
    assert_eq!(default.issue.expect("issue").team_key, "DEN");

    for repository in REVIEWED_REPOSITORIES {
        let resolved = registry
            .resolve(&request(Some(repository)))
            .unwrap_or_else(|error| panic!("{repository} must resolve: {error}"));
        assert_eq!(resolved.repository.to_ascii_lowercase(), repository.to_ascii_lowercase());
        assert_eq!(resolved.write_policy, WritePolicy::DraftPullRequest);
        assert_eq!(resolved.linear_project_id, "e1db74d7-4fa3-4580-851d-ca8fc8145127");
    }
}

#[test]
fn generated_legacy_unknown_and_cross_project_targets_fail_closed() {
    let registry = registry();

    for repository in [
        "benefactor-cc/benefactor-cc.github.io",
        "benefactor-cc/benfactor-cc",
        "benefactor-cc/not-reviewed",
        "memebank/mbk-api",
        "https://github.com/benefactor-cc/backend.rs",
        "benefactor-cc/backend.rs/subdir",
        "../benefactor-cc/backend.rs",
    ] {
        let result = registry.resolve(&request(Some(repository)));
        assert!(
            matches!(
                result,
                Err(RegistryError::RepositoryNotAllowed | RegistryError::InvalidRepository)
            ),
            "{repository:?} must fail closed, got {result:?}",
        );
    }
}

#[test]
fn repository_write_still_requires_a_valid_denman_issue() {
    let registry = registry();

    let mut missing = request(Some("benefactor-cc/backend.rs"));
    missing.linear_issue_identifier = None;
    assert!(registry.resolve(&missing).is_err());

    let mut foreign = request(Some("benefactor-cc/backend.rs"));
    foreign.linear_issue_identifier = Some("ENG-3009".to_string());
    assert!(matches!(
        registry.resolve(&foreign),
        Err(RegistryError::IssueTeamMismatch)
    ));
}
