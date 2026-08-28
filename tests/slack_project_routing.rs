//! Routing contract for the thirteen project channels activated on
//! 2026-08-01 (DEN-1298).
//!
//! `slack_project_bindings` is the fail-closed gate between a Slack channel and
//! the Linear project plus GitHub repository an agent run is allowed to touch.
//! Its unit tests cover the validators in isolation; this suite covers the
//! contract the deployment actually depends on — that every activated channel
//! resolves to its *own* project, and that everything else is refused.
//!
//! Deliberately offline: no Slack, no network, no fixtures on disk.

use std::collections::BTreeSet;

use ai_agent_bridge::slack_project_bindings::{
    AgentMode, RegistryError, RequestedCapability, ResolveRequest, SlackProjectRegistry,
    WritePolicy,
};
use serde_json::{json, Value};

/// The workspace hosting `alex-main-agent` (`A0BMBAMM5NJ`).
const WORKSPACE: &str = "T01B3C83PMK";
const OPERATOR: &str = "U01OPERATOR";
const OPERATOR_GROUP: &str = "S01MAINTAINERS";

/// Channel name, Slack channel id, owning GitHub org, repository.
///
/// Names are documentation: the registry keys on immutable ids precisely so a
/// renamed or misspelled channel cannot silently inherit another project's
/// permissions.
const CHANNELS: [(&str, &str, &str, &str); 13] = [
    ("3fa-app", "C3FAAPP00001", "3fa-app", "3fa-backend.rs"),
    ("cliptown", "CCLIPTOWN001", "cliptown", "cliptown-monorepo"),
    (
        "benefactor-cc",
        "CBENEFACTOR1",
        "benefactor-cc",
        "benefactor-backend.rs",
    ),
    ("athlet-o", "CATHLETO0001", "athlet-o", "athleto-backend.rs"),
    ("memebank", "CMEMEBANK001", "memebank", "memebank-monorepo"),
    (
        "scintilla-run",
        "CSCINTILLA01",
        "scintilla-run",
        "scintilla-run",
    ),
    (
        "quaestor-ledger",
        "CQUAESTOR001",
        "quaestor-ledger",
        "quaestor-ledger",
    ),
    (
        "daedalus-fab",
        "CDAEDALUS001",
        "daedalus-fab",
        "daedalus-fab",
    ),
    ("hypesiege", "CHYPESIEGE01", "hypesiege", "hypesiege"),
    ("streempilot", "CSTREEMPLT01", "streempilot", "streempilot"),
    (
        "shared-auth",
        "CSHAREDAUTH",
        "shared-auth",
        "shared-auth-clients",
    ),
    ("opto-sync", "COPTOSYNC001", "opto-sync", "opto-sync"),
    ("voxletra", "CVOXLETRA001", "voxletra", "voxletra"),
];

fn binding(channel_id: &str, org: &str, repository: &str, write_policy: &str) -> Value {
    json!({
        "workspace_id": WORKSPACE,
        "channel_id": channel_id,
        "linear_team_id": "team-eb8ab169",
        "linear_team_key": "DEN",
        "linear_project_id": format!("project-{channel_id}"),
        "default_repository": format!("{org}/{repository}"),
        "repository_allowlist": [format!("{org}/{repository}")],
        "default_agent_mode": "claude",
        "allowed_agent_modes": ["claude", "chatgpt"],
        "allowed_user_ids": [OPERATOR],
        "allowed_user_group_ids": [OPERATOR_GROUP],
        "write_policy": write_policy,
        "budget_policy": {
            "max_concurrent_runs": 2,
            "max_runtime_secs": 900,
            "max_tokens": 500_000,
            "max_spend_cents": 500,
            "max_retries": 2
        },
        "updated_by": OPERATOR,
        "updated_at": "2026-08-01T00:00:00Z"
    })
}

fn registry_document(bindings: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({ "schema_version": 1, "bindings": bindings })).expect("serialize")
}

fn canonical_registry() -> SlackProjectRegistry {
    let bindings = CHANNELS
        .iter()
        .map(|(_, channel_id, org, repository)| {
            binding(channel_id, org, repository, "draft_pull_request")
        })
        .collect();
    SlackProjectRegistry::from_json(&registry_document(bindings)).expect("canonical registry loads")
}

fn request(channel_id: &str) -> ResolveRequest {
    ResolveRequest {
        workspace_id: WORKSPACE.to_string(),
        channel_id: channel_id.to_string(),
        user_id: OPERATOR.to_string(),
        user_group_ids: BTreeSet::new(),
        requested_repository: None,
        requested_agent_mode: None,
        requested_capability: RequestedCapability::ReadOnly,
        linear_issue_identifier: None,
    }
}

#[test]
fn every_activated_channel_resolves_to_its_own_project_and_repository() {
    let registry = canonical_registry();
    assert_eq!(registry.binding_count(), 13, "all thirteen channels bound");

    let mut seen_projects = BTreeSet::new();
    let mut seen_repositories = BTreeSet::new();

    for (name, channel_id, org, repository) in CHANNELS {
        let resolved = registry
            .resolve(&request(channel_id))
            .unwrap_or_else(|error| panic!("#{name} must resolve, got {error}"));

        assert_eq!(resolved.channel_id, channel_id);
        assert_eq!(
            resolved.repository,
            format!("{org}/{repository}"),
            "#{name} resolved to the wrong repository",
        );
        assert_eq!(resolved.linear_team_key, "DEN");
        assert_eq!(resolved.agent_mode, AgentMode::Claude);

        // Two channels sharing a project or repository would let work posted in
        // one product's channel land in another product's tracker or codebase.
        assert!(
            seen_projects.insert(resolved.linear_project_id.clone()),
            "#{name} shares a Linear project with another channel",
        );
        assert!(
            seen_repositories.insert(resolved.repository.clone()),
            "#{name} shares a repository with another channel",
        );
    }
}

#[test]
fn the_misspelled_daedalus_channel_is_refused() {
    // DEN-1298 calls this out by name: #daadalus-fab is a typo that must never
    // inherit #daedalus-fab's routing. Because the registry keys on channel id,
    // a look-alike name simply carries an id nobody bound.
    let registry = canonical_registry();

    let resolved = registry.resolve(&request("CDAEDALUS001"));
    assert!(resolved.is_ok(), "the canonical channel must still resolve");

    let typo = registry.resolve(&request("CDAADALUS001"));
    assert!(
        matches!(typo, Err(RegistryError::UnmappedChannel)),
        "the misspelled channel must be refused, got {typo:?}",
    );
}

#[test]
fn unmapped_and_foreign_workspace_channels_fail_closed() {
    let registry = canonical_registry();

    assert!(matches!(
        registry.resolve(&request("CNOTBOUND001")),
        Err(RegistryError::UnmappedChannel)
    ));

    // A correct channel id under someone else's workspace must not resolve.
    let mut foreign = request("C3FAAPP00001");
    foreign.workspace_id = "T09OTHERWSP".to_string();
    assert!(
        matches!(
            registry.resolve(&foreign),
            Err(RegistryError::UnmappedChannel)
        ),
        "a channel id must not resolve outside its own workspace",
    );
}

#[test]
fn a_channel_cannot_reach_another_projects_repository() {
    // The organization-escape case: #memebank asking for cliptown's repository.
    let registry = canonical_registry();

    let mut escape = request("CMEMEBANK001");
    escape.requested_repository = Some("cliptown/cliptown-monorepo".to_string());
    assert!(
        matches!(
            registry.resolve(&escape),
            Err(RegistryError::RepositoryNotAllowed)
        ),
        "a channel must not dispatch into another project's repository",
    );

    // Its own repository is still reachable when named explicitly, including
    // with different casing, since the registry normalizes.
    let mut own = request("CMEMEBANK001");
    own.requested_repository = Some("MemeBank/MemeBank-Monorepo".to_string());
    assert_eq!(
        registry.resolve(&own).expect("own repository").repository,
        "memebank/memebank-monorepo",
    );
}

#[test]
fn repository_values_that_are_not_owner_slash_repo_are_rejected() {
    let registry = canonical_registry();
    for candidate in [
        "https://github.com/memebank/memebank-monorepo",
        "memebank/memebank-monorepo.git",
        "memebank",
        "memebank/sub/path",
        "../memebank/memebank-monorepo",
    ] {
        let mut escape = request("CMEMEBANK001");
        escape.requested_repository = Some(candidate.to_string());
        assert!(
            matches!(
                registry.resolve(&escape),
                Err(RegistryError::InvalidRepository | RegistryError::RepositoryNotAllowed)
            ),
            "{candidate:?} must not be accepted as a repository",
        );
    }
}

#[test]
fn only_authorized_principals_may_dispatch() {
    let registry = canonical_registry();

    let mut stranger = request("C3FAAPP00001");
    stranger.user_id = "U09STRANGER".to_string();
    assert!(matches!(
        registry.resolve(&stranger),
        Err(RegistryError::UnauthorizedPrincipal)
    ));

    // Group membership is an accepted second path to authorization.
    let mut by_group = stranger.clone();
    by_group.user_group_ids = [OPERATOR_GROUP.to_string()].into_iter().collect();
    assert!(registry.resolve(&by_group).is_ok());

    // But an unrelated group is not.
    let mut wrong_group = stranger;
    wrong_group.user_group_ids = ["S09CONTRACTORS".to_string()].into_iter().collect();
    assert!(matches!(
        registry.resolve(&wrong_group),
        Err(RegistryError::UnauthorizedPrincipal)
    ));
}

#[test]
fn write_capability_is_bounded_by_each_channels_policy() {
    let read_only = SlackProjectRegistry::from_json(&registry_document(vec![binding(
        "CREADONLY001",
        "memebank",
        "memebank-monorepo",
        "read_only",
    )]))
    .expect("registry");
    let linear_only = SlackProjectRegistry::from_json(&registry_document(vec![binding(
        "CLINEARONLY1",
        "memebank",
        "memebank-monorepo",
        "linear_only",
    )]))
    .expect("registry");
    let drafting = canonical_registry();

    let cases = [
        (
            &read_only,
            "CREADONLY001",
            [true, false, false],
            WritePolicy::ReadOnly,
        ),
        (
            &linear_only,
            "CLINEARONLY1",
            [true, true, false],
            WritePolicy::LinearOnly,
        ),
        (
            &drafting,
            "C3FAAPP00001",
            [true, true, true],
            WritePolicy::DraftPullRequest,
        ),
    ];

    for (registry, channel_id, expected, policy) in cases {
        for (capability, allowed) in [
            RequestedCapability::ReadOnly,
            RequestedCapability::LinearWrite,
            RequestedCapability::RepositoryWrite,
        ]
        .into_iter()
        .zip(expected)
        {
            let mut ask = request(channel_id);
            ask.requested_capability = capability;
            let resolved = registry.resolve(&ask);
            assert_eq!(
                resolved.is_ok(),
                allowed,
                "{policy:?} handled {capability:?} incorrectly",
            );
            if let Ok(resolved) = resolved {
                assert_eq!(resolved.write_policy, policy);
            }
        }
    }
}

#[test]
fn agent_modes_outside_a_channels_allowlist_are_refused() {
    let registry = canonical_registry();

    for mode in [AgentMode::Claude, AgentMode::Chatgpt] {
        let mut ask = request("COPTOSYNC001");
        ask.requested_agent_mode = Some(mode);
        assert_eq!(
            registry.resolve(&ask).expect("allowed mode").agent_mode,
            mode
        );
    }

    for mode in [
        AgentMode::BothParallel,
        AgentMode::BothSequential,
        AgentMode::Review,
    ] {
        let mut ask = request("COPTOSYNC001");
        ask.requested_agent_mode = Some(mode);
        assert!(
            matches!(
                registry.resolve(&ask),
                Err(RegistryError::AgentModeNotAllowed)
            ),
            "{mode:?} is not in this channel's allowlist",
        );
    }
}

#[test]
fn a_linear_issue_from_another_team_is_refused() {
    let registry = canonical_registry();

    let mut ok = request("CCLIPTOWN001");
    ok.linear_issue_identifier = Some("DEN-1298".to_string());
    let resolved = ok_issue(&registry, ok);
    assert_eq!(resolved.number, 1298);
    assert_eq!(resolved.team_key, "DEN");

    let mut foreign = request("CCLIPTOWN001");
    foreign.linear_issue_identifier = Some("ENG-1298".to_string());
    assert!(matches!(
        registry.resolve(&foreign),
        Err(RegistryError::IssueTeamMismatch)
    ));

    for malformed in ["DEN", "DEN-", "DEN-0", "DEN-01", "-1", "den-1", "DEN-1x"] {
        let mut bad = request("CCLIPTOWN001");
        bad.linear_issue_identifier = Some(malformed.to_string());
        assert!(
            matches!(
                registry.resolve(&bad),
                Err(RegistryError::InvalidIssueIdentifier)
            ),
            "{malformed:?} must not parse as a Linear identifier",
        );
    }
}

fn ok_issue(
    registry: &SlackProjectRegistry,
    request: ResolveRequest,
) -> ai_agent_bridge::slack_project_bindings::LinearIssueRef {
    registry
        .resolve(&request)
        .expect("resolve")
        .issue
        .expect("issue")
}

#[test]
fn a_channel_bound_twice_is_rejected_at_load() {
    // Two bindings for one channel would make routing order-dependent, so the
    // whole document is refused rather than one winning silently.
    let duplicate = registry_document(vec![
        binding("CMEMEBANK001", "memebank", "memebank-monorepo", "read_only"),
        binding(
            "CMEMEBANK001",
            "cliptown",
            "cliptown-monorepo",
            "draft_pull_request",
        ),
    ]);
    assert!(matches!(
        SlackProjectRegistry::from_json(&duplicate),
        Err(RegistryError::DuplicateBinding)
    ));
}

#[test]
fn a_binding_whose_default_repository_is_not_allowlisted_is_rejected() {
    let mut malformed = binding("CMEMEBANK001", "memebank", "memebank-monorepo", "read_only");
    malformed["default_repository"] = json!("cliptown/cliptown-monorepo");

    assert!(matches!(
        SlackProjectRegistry::from_json(&registry_document(vec![malformed])),
        Err(RegistryError::DefaultRepositoryNotAllowed)
    ));
}

#[test]
fn an_unknown_registry_field_is_rejected_rather_than_ignored() {
    // The document is a security control, so a misspelled key must fail loudly
    // instead of silently leaving a policy at its default.
    let mut sneaky = binding("CMEMEBANK001", "memebank", "memebank-monorepo", "read_only");
    sneaky["write_polcy"] = json!("draft_pull_request");

    assert!(matches!(
        SlackProjectRegistry::from_json(&registry_document(vec![sneaky])),
        Err(RegistryError::Json(_))
    ));
}
