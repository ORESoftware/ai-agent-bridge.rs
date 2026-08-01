# alex-main-agent pilot binding

Tracking issue: `DEN-1041`

This document records the reviewed, non-secret identifiers for the first ORESoftware Slack command pilot. It does not contain the Slack bot token, signing secret, app configuration token, bridge bearer, or coordinator bearer.

## Slack identity

| Resource | Stable identifier |
|---|---|
| App | `alex-main-agent` |
| App ID | `A0BMBAMM5NJ` |
| Workspace/team ID | `T01B3C83PMK` |
| Pilot channel | `#oresoftware` |
| Pilot channel ID | `C0BKP2N3LG7` |
| Pilot operator | Alex Mills |
| Pilot operator user ID | `U01AZNU2LJ2` |

The Slack app is installed in the workspace. The reviewed command and interaction configuration lives in `slack-app/manifest.yaml`.

## Public request surface

The reviewed manifest sends Slack requests only to these TLS endpoints:

```text
https://api.fiducia.cloud/slack/commands/ores-claude
https://api.fiducia.cloud/slack/commands/ores-chatgpt
https://api.fiducia.cloud/slack/interactions
```

The application must verify Slack signatures and request freshness before parsing or journaling a request. No gateway authentication cookie or operator bearer may be required on these three Slack-signed endpoints.

## Linear routing

| Resource | Stable identifier |
|---|---|
| Linear team | Denman (`DEN`) |
| Linear team ID | `eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc` |
| Owning project | `github.com/ORESoftware` |
| Owning project ID | `7abf8be2-ffa5-4507-bd09-43aa59ca8718` |
| AI Agent Run Queue project ID | `72e891e2-603d-4903-8d08-bd06d204520f` |

The initial repository allowlist is:

```text
ORESoftware/ai-agent-bridge.rs
ORESoftware/ai-agent-coordinator.rs
ORESoftware/k8s-cluster
```

The initial write policy is `draft_pull_request`. Only `U01AZNU2LJ2` is authorized during the pilot. Broader users, channels, repositories, or user groups require a reviewed registry change.

## Applying the manifest

Slack app manifests replace the app configuration as a whole. Export or review the current app manifest before applying `slack-app/manifest.yaml`, then validate the complete merged document.

Using the Slack app settings UI:

1. Open app `A0BMBAMM5NJ`.
2. Open **App Manifest**.
3. Merge and validate `slack-app/manifest.yaml`.
4. Save the manifest and reinstall the app if Slack reports changed scopes.

Using an app configuration token:

```bash
manifest_json="$(yq -o=json '.' slack-app/manifest.yaml)"
slack api apps.manifest.validate \
  --team T01B3C83PMK \
  --token "$SLACK_CONFIG_TOKEN" \
  "$(jq -n --arg manifest "$manifest_json" '{manifest:$manifest}')"

slack api apps.manifest.update \
  --team T01B3C83PMK \
  --token "$SLACK_CONFIG_TOKEN" \
  "$(jq -n --arg app_id A0BMBAMM5NJ --arg manifest "$manifest_json" '{app_id:$app_id,manifest:$manifest}')"
```

App configuration tokens are short-lived and must remain outside Git, logs, Linear, and Slack messages.

## Secret-store contract

The Kubernetes deployment must source these values from External Secrets:

- `SLACK_BOT_TOKEN`;
- `SLACK_SIGNING_SECRET`;
- `SLACK_BRIDGE_BEARER` when live bridge dispatch is enabled;
- `SLACK_COORDINATOR_BEARER` when live coordinator dispatch is enabled.

The first cluster rollout remains `SLACK_COMMAND_DRY_RUN=true`. It may read the five latest approved non-bot messages and post a metadata-only dry-run acknowledgement, but it must not create bridge workflows, coordinator jobs, Linear records, GitHub branches, or pull requests until the live activation gates in `docs/slack-ores-commands.md` are satisfied.
