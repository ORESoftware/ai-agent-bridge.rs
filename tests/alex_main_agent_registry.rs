use std::collections::BTreeSet;

use ai_agent_bridge::slack_project_bindings::{
    RegistryError, RequestedCapability, ResolveRequest, SlackProjectRegistry, WritePolicy,
};

const REGISTRY: &[u8] = include_bytes!("../config/alex-main-agent.channels.json");
const WORKSPACE_ID: &str = "T01B3C83PMK";
const ALEX_USER_ID: &str = "U01AZNU2LJ2";

fn request(channel_id: &str, user_id: &str) -> ResolveRequest {
    ResolveRequest {
        workspace_id: WORKSPACE_ID.to_string(),
        channel_id: channel_id.to_string(),
        user_id: user_id.to_string(),
        user_group_ids: BTreeSet::new(),
        requested_repository: None,
        requested_agent_mode: None,
        requested_capability: RequestedCapability::RepositoryWrite,
        linear_issue_identifier: None,
    }
}

#[test]
fn alex_main_agent_registry_parses_all_project_bindings() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    assert_eq!(registry.binding_count(), 15);
}

#[test]
fn oresoftware_pilot_resolves_only_reviewed_control_plane_repositories() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let mut request = request("C0BKP2N3LG7", ALEX_USER_ID);
    request.requested_repository = Some("ORESoftware/k8s-cluster".to_string());
    request.linear_issue_identifier = Some("DEN-1298".to_string());

    let context = registry
        .resolve(&request)
        .expect("pilot route must resolve");
    assert_eq!(
        context.linear_project_id,
        "7abf8be2-ffa5-4507-bd09-43aa59ca8718"
    );
    assert_eq!(context.repository, "oresoftware/k8s-cluster");
    assert_eq!(context.write_policy, WritePolicy::DraftPullRequest);
    assert_eq!(
        context.issue.expect("issue must be parsed").identifier,
        "DEN-1298"
    );

    request.requested_repository = Some("ORESoftware/unreviewed-repository".to_string());
    assert!(matches!(
        registry.resolve(&request),
        Err(RegistryError::RepositoryNotAllowed)
    ));
}

#[test]
fn hypesiege_resolves_to_the_allowlisted_draft_pr_target() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let mut request = request("C0BMF6JDSHX", ALEX_USER_ID);
    request.linear_issue_identifier = Some("DEN-1280".to_string());

    let context = registry.resolve(&request).expect("route must resolve");

    assert_eq!(context.linear_team_key, "DEN");
    assert_eq!(
        context.linear_project_id,
        "cd247cb1-870b-471e-89f6-9484df19e798"
    );
    assert_eq!(context.repository, "hypesiege/hypesiege-mcp-server.rs");
    assert_eq!(context.write_policy, WritePolicy::DraftPullRequest);
    assert_eq!(
        context.issue.expect("issue must be parsed").identifier,
        "DEN-1280"
    );
}

#[test]
fn misspelled_daedalus_channel_is_unmapped() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let result = registry.resolve(&request("C0BMB9GSSKY", ALEX_USER_ID));

    assert!(matches!(result, Err(RegistryError::UnmappedChannel)));
}

#[test]
fn fanwaave_routes_only_reviewed_repositories_as_draft_pull_requests() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let mut request = request("C0BN3FMEJGL", ALEX_USER_ID);
    request.linear_issue_identifier = Some("DEN-1874".to_string());

    let default_context = registry
        .resolve(&request)
        .expect("Fanwaave route must resolve");
    assert_eq!(default_context.linear_team_key, "DEN");
    assert_eq!(
        default_context.linear_project_id,
        "d765e227-5726-42c8-8643-a8bd9e5a9a8c"
    );
    assert_eq!(
        default_context.repository,
        "fanwaave/push-notification-server.rs"
    );
    assert_eq!(default_context.write_policy, WritePolicy::DraftPullRequest);

    request.requested_repository = Some("fanwaave/.github".to_string());
    let governance_context = registry
        .resolve(&request)
        .expect("Fanwaave governance route must resolve");
    assert_eq!(governance_context.repository, "fanwaave/.github");

    request.requested_repository = Some("ORESoftware/k8s-cluster".to_string());
    assert!(matches!(
        registry.resolve(&request),
        Err(RegistryError::RepositoryNotAllowed)
    ));
}

#[test]
fn unauthorized_user_cannot_dispatch_repository_work() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let result = registry.resolve(&request("C0BL6BEDYFK", "U_NOT_ALLOWED"));

    assert!(matches!(result, Err(RegistryError::UnauthorizedPrincipal)));
}

#[test]
fn repository_escape_is_rejected() {
    let registry = SlackProjectRegistry::from_json(REGISTRY).expect("registry must be valid");
    let mut request = request("C0BMBARQ7N2", ALEX_USER_ID);
    request.requested_repository = Some("ORESoftware/ai-agent-bridge.rs".to_string());

    let result = registry.resolve(&request);

    assert!(matches!(result, Err(RegistryError::RepositoryNotAllowed)));
}
