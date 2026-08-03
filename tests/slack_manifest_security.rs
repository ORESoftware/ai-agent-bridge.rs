use std::collections::BTreeSet;

const REQUIRED_BOT_SCOPES: [&str; 5] = [
    "commands",
    "chat:write",
    "channels:history",
    "groups:history",
    "usergroups:read",
];

fn bot_scopes(manifest: &str) -> Vec<&str> {
    let mut in_oauth_config = false;
    let mut in_scopes = false;
    let mut in_bot = false;
    let mut scopes = Vec::new();

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "oauth_config:" {
            in_oauth_config = true;
            continue;
        }
        if !in_oauth_config {
            continue;
        }
        if !line.starts_with(' ') && !trimmed.is_empty() {
            break;
        }
        if trimmed == "scopes:" {
            in_scopes = true;
            continue;
        }
        if in_scopes && trimmed == "bot:" {
            in_bot = true;
            continue;
        }
        if in_bot {
            if let Some(scope) = trimmed.strip_prefix("- ") {
                scopes.push(scope);
                continue;
            }
            if !trimmed.is_empty() && !line.starts_with("      ") {
                break;
            }
        }
    }

    scopes
}

#[test]
fn reviewed_manifest_has_the_exact_required_bot_scope_set() {
    let manifest = include_str!("../slack-app/manifest.yaml");
    let scopes = bot_scopes(manifest);
    let unique = scopes.iter().copied().collect::<BTreeSet<_>>();
    let expected = REQUIRED_BOT_SCOPES.into_iter().collect::<BTreeSet<_>>();

    assert_eq!(scopes.len(), unique.len(), "bot scopes must not be duplicated");
    assert_eq!(unique, expected, "bot scope drift requires security review");
}

#[test]
fn manifest_keeps_tokens_and_secret_values_out_of_source_control() {
    let manifest = include_str!("../slack-app/manifest.yaml");

    for forbidden in [
        "xoxb-",
        "xapp-",
        "xoxp-",
        "signing_secret",
        "bot_token",
        "client_secret",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "manifest must not contain {forbidden}"
        );
    }
}

#[test]
fn user_group_authorization_cannot_drift_from_its_runtime_dependency() {
    let runtime = include_str!("../src/slack_commands_parts/part4.rs");
    let manifest = include_str!("../slack-app/manifest.yaml");

    assert!(runtime.contains("usergroups.list"));
    assert!(manifest.lines().any(|line| line.trim() == "- usergroups:read"));
}
